use arrow_flight::flight_service_server::FlightServiceServer;
use clap::{Args, Command};
use deltaquery::configs::DQConfig;
use deltaquery::servers::delta::FlightSqlServiceDelta;
use deltaquery::servers::simple::FlightSqlServiceSimple;
use deltaquery::state::DQState;
use env_logger::Builder;
use std::env;
use std::fs::File;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::time;
use tonic::transport::Server;
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

#[derive(Debug, Args)]
pub struct DQOption {
    #[arg(short = 'c', long, help = "Config file")]
    config: Option<String>,

    #[arg(short = 'l', long, help = "Log filters")]
    logfilter: Option<String>,
}

fn handle_state(state: Arc<Mutex<DQState>>) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(
            match env::var("DELTAQUERY_UPDATE_INTERVAL") {
                Ok(value) => value.parse().unwrap(),
                Err(_) => 60,
            },
        ));

        loop {
            {
                let mut state = state.lock().await;

                for (_, table) in state.get_tables() {
                    let time0 = Instant::now();

                    let _ = table.update().await;

                    log::info!("updated for {} milliseconds", time0.elapsed().as_millis());
                }
            }

            interval.tick().await;
        }
    });
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cmd = Command::new("DeltaQuery");
    let cmd = DQOption::augment_args(cmd);
    let args = cmd.get_matches();

    if let Some(filter) = args.get_one::<String>("logfilter") {
        Builder::new().parse_filters(filter.as_str()).init();
    } else {
        env_logger::init();
    }

    let config = match args.get_one::<String>("config") {
        Some(config) => {
            let f = File::open(config).expect("could not open file");
            let c: DQConfig = serde_yaml::from_reader(f).expect("could not parse yaml");

            c
        }
        None => panic!("could not find config file"),
    };

    let state = Arc::new(Mutex::new(DQState::new(config.clone()).await));
    handle_state(state.clone());

    let mut server = if let Some(tls_config) = config.tls.as_ref() {
        let server_cert = std::fs::read_to_string(tls_config.server_cert.clone())?;
        let server_key = std::fs::read_to_string(tls_config.server_key.clone())?;
        let client_cert = std::fs::read_to_string(tls_config.client_cert.clone())?;

        let tls_config = ServerTlsConfig::new()
            .identity(Identity::from_pem(&server_cert, &server_key))
            .client_ca_root(Certificate::from_pem(&client_cert));

        Server::builder().tls_config(tls_config)?
    } else {
        Server::builder()
    };

    let listen = config.listen.parse()?;

    log::info!("listening on {:?}", listen);

    match config.server.as_str() {
        "simple" => {
            server
                .add_service(FlightServiceServer::new(
                    FlightSqlServiceSimple::new(state.clone()).await,
                ))
                .serve(listen)
                .await?;
        }
        _ => {
            server
                .add_service(FlightServiceServer::new(
                    FlightSqlServiceDelta::new(state.clone()).await,
                ))
                .serve(listen)
                .await?;
        }
    }

    Ok(())
}
