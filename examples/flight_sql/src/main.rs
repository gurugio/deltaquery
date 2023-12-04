use anyhow::Result;
use arrow_array::RecordBatch;
use arrow_cast::pretty::pretty_format_batches;
use arrow_flight::{sql::client::FlightSqlServiceClient, FlightInfo};
use arrow_schema::Schema;
use clap::{Args, Command};
use env_logger::Builder;
use futures::stream::FuturesOrdered;
use futures::{StreamExt, TryStreamExt};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use std::fs::File;
use std::{sync::Arc, time::Duration};
use tonic::{metadata::MetadataMap, transport::Endpoint};

#[derive(Debug, Args)]
pub struct DQOption {
    #[arg(short = 'l', long, help = "Log filters")]
    logfilter: Option<String>, // flight_sql=info

    #[arg(short = 'c', long, help = "Target command")]
    command: String, // statement-query

    #[arg(short = 's', long, help = "Target host")]
    host: String, // 127.0.0.1

    #[arg(short = 'p', long, help = "Target port")]
    port: u16, // 32010

    #[arg(short = 't', long, help = "Target protocol")]
    protocol: String, // http

    #[arg(short = 'a', long, help = "Target authorization")]
    authorization: Option<String>, // "Basic YWRtaW46YWRtaW4K"

    #[arg(short = 'u', long, help = "Target username")]
    username: Option<String>,

    #[arg(short = 'w', long, help = "Target password")]
    password: Option<String>,

    #[arg(short = 'd', long, help = "Target database")]
    database: Option<String>,

    #[arg(short = 'q', long, help = "Target query")]
    query: String, // select date,hour,name,score from delta.default.test0

    #[arg(short = 'r', long, help = "Target output")]
    output: Option<String>, // pretty
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cmd = Command::new("FlightSQL"); // What is clap? Command line argument parser
    let cmd = DQOption::augment_args(cmd);
    let args = cmd.get_matches();

    if let Some(filter) = args.get_one::<String>("logfilter") {
        Builder::new().parse_filters(filter.as_str()).init();
    } else {
        env_logger::init();
    }

    let command = args.get_one::<String>("command").unwrap();
    let host = args.get_one::<String>("host").unwrap();
    let port = args.get_one::<u16>("port").unwrap();
    let protocol = args.get_one::<String>("protocol").unwrap();
    let query = args.get_one::<String>("query").unwrap().to_owned(); // should be String for later use

    // Client connects the server via gRPC
    let endpoint = Endpoint::new(format!("{}://{}:{}", protocol, host, port))
        .unwrap()
        .connect_timeout(Duration::from_secs(60 * 10))
        .timeout(Duration::from_secs(60 * 10))
        .tcp_nodelay(true)
        .tcp_keepalive(Option::Some(Duration::from_secs(60 * 60)))
        .http2_keep_alive_interval(Duration::from_secs(60 * 10))
        .keep_alive_timeout(Duration::from_secs(60 * 10))
        .keep_alive_while_idle(true);

    let channel = endpoint.connect().await.unwrap();

    // FlightSqlServiceClient document says that it should be created with gRPC channel
    // which is created with tonic::transport::Endpoint.
    // Arrow Flight is a gRPC framework for high-performance data transfer based on Apache Arrow.
    // Arrow flight SQL is a protocol for interacting with SQL database using the Arrow in-memory format
    // and the Flight RPC framework. That's why FlightSqlServiceClient needs gRPC channel.
    let mut client = FlightSqlServiceClient::new(channel);

    // Where can I get the spec of the header?
    // Which data should I set to the header?
    if let Some(database) = args.get_one::<String>("database") {
        client.set_header("x-flight-sql-database", database);
    }
    if let Some(authorization) = args.get_one::<String>("authorization") {
        client.set_header("authorization", authorization);
    }
    if let (Some(username), Some(password)) = (
        args.get_one::<String>("username"),
        args.get_one::<String>("password"),
    ) {
        let token = client.handshake(&username, &password).await.unwrap();
        client.set_token(String::from_utf8(token.to_vec()).unwrap());

        log::info!("handshake={:#?}", token);
    }

    // statement-query or prepared-statement-query: Which one should I use?
    // What is the difference between them?
    // What is the difference between client.execute and client.prepare?
    let flight_info = match command.as_str() {
        "statement-query" => client.execute(query, None).await.unwrap(),
        "prepared-statement-query" => {
            let mut prepared_statement = client.prepare(query, None).await.unwrap();
            log::info!("prepare={:#?}", prepared_statement);
            prepared_statement.execute().await.unwrap()
        }
        _ => unimplemented!(),
    };

    log::info!("execute={:#?}", flight_info);

    let batches = fetch_flight_data(flight_info, args.get_one::<String>("authorization"))
        .await
        .unwrap();

    if let Some(output) = args.get_one::<String>("output") {
        match output.as_str() {
            "pretty" => {
                log::info!("{}", pretty_format_batches(batches.as_slice()).unwrap());
            }
            "dump" => {
                log::info!("{:?}", batches);
            }
            "parquet" => {
                let file = File::create("output.parquet").unwrap();

                let props = WriterProperties::builder()
                    .set_compression(Compression::UNCOMPRESSED)
                    .build();

                let batch0 = batches.first().unwrap();

                let mut writer = ArrowWriter::try_new(file, batch0.schema(), Some(props)).unwrap();
                for batch in batches {
                    writer.write(&batch).expect("could not write batch");
                }
                writer.close().unwrap();
            }
            _ => unimplemented!(),
        }
    } else {
        log::info!(
            "batches={:?}",
            batches.iter().map(|b| b.num_rows()).sum::<usize>()
        );
    }

    Ok(())
}

async fn fetch_flight_data(
    info: FlightInfo,
    authorization: Option<&String>,
) -> Result<Vec<RecordBatch>> {
    let schema = Arc::new(Schema::try_from(info.clone())?);
    let mut batches = Vec::with_capacity(info.endpoint.len() + 1);
    batches.push(RecordBatch::new_empty(schema));

    let mut futures = FuturesOrdered::new();

    for endpoint in info.endpoint {
        futures.push_back(async move {
            let location = endpoint.location.first().unwrap();
            let ticket = endpoint.ticket.unwrap();

            let endpoint = Endpoint::new(location.uri.clone())
                .unwrap()
                .connect_timeout(Duration::from_secs(60 * 10))
                .timeout(Duration::from_secs(60 * 10))
                .tcp_nodelay(true)
                .tcp_keepalive(Option::Some(Duration::from_secs(60 * 60)))
                .http2_keep_alive_interval(Duration::from_secs(60 * 10))
                .keep_alive_timeout(Duration::from_secs(60 * 10))
                .keep_alive_while_idle(true);

            let channel = endpoint.connect().await.unwrap();

            let mut client = FlightSqlServiceClient::new(channel);
            if let Some(authorization) = authorization {
                client.set_header("authorization", authorization);
            }

            client.do_get(ticket).await
        });
    }

    while let Some(result) = futures.next().await {
        match result {
            Ok(mut flight_data) => {
                let items: Vec<_> = (&mut flight_data).try_collect().await.unwrap();

                log_flight_metadata(flight_data.headers(), "headers");

                log::info!(
                    "fetch=rows={:?}",
                    items.iter().map(|b| b.num_rows()).sum::<usize>()
                );

                batches.extend(items);
            }
            Err(err) => {
                log::error!("could not get flight data: {:?}", err);
            }
        }
    }

    Ok(batches)
}

fn log_flight_metadata(map: &MetadataMap, what: &'static str) {
    for k_v in map.iter() {
        match k_v {
            tonic::metadata::KeyAndValueRef::Ascii(k, v) => {
                log::info!(
                    "{}: {}={}",
                    what,
                    k.as_str(),
                    v.to_str().unwrap_or("<invalid>"),
                );
            }
            tonic::metadata::KeyAndValueRef::Binary(k, v) => {
                log::info!(
                    "{}: {}={}",
                    what,
                    k.as_str(),
                    String::from_utf8_lossy(v.as_ref()),
                );
            }
        }
    }
}
