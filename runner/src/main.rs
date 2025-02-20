mod error;
mod runner;

use clap::Parser;

use error::Error;

use futures::{FutureExt, TryFutureExt};
use runner::ClientType;
use signal_hook::consts::*;
use signal_hook_tokio::Signals;
use std::{fmt::Debug, path::PathBuf};

use crate::runner::{retry_with_log_async, subxt_api, Runner};

#[derive(Parser, Debug, Clone)]
#[clap(version, author, about, trailing_var_arg = true)]
pub struct Opts {
    /// Client to run, one of: vault, oracle, faucet. Default is `vault`.
    #[clap(long, default_value = "vault")]
    pub client_type: ClientType,

    /// Parachain websocket URL.
    #[clap(long)]
    pub parachain_ws: String,

    /// Download path for the client executable.
    #[clap(long, default_value = ".")]
    pub download_path: PathBuf,

    /// CLI arguments to pass to the client executable.
    pub client_args: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, log::LevelFilter::Info.as_str()),
    );
    let opts: Opts = Opts::parse();
    let rpc_client = retry_with_log_async(
        || subxt_api(&opts.parachain_ws).into_future().boxed(),
        "Error fetching executable".to_string(),
    )
    .await?;
    log::info!("Connected to the parachain");

    let runner = Runner::new(rpc_client, opts);
    let shutdown_signals = Signals::new(&[SIGHUP, SIGTERM, SIGINT, SIGQUIT])?;
    Runner::run(Box::new(runner), shutdown_signals).await?;
    Ok(())
}
