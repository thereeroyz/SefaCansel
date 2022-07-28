use crate::error::Error;
use bytes::Bytes;
use codec::{Decode, Encode};
use jsonrpsee::{
    core::client::{Client as WsClient, ClientT},
    rpc_params,
    ws_client::WsClientBuilder,
};
use reqwest::Url;
use sp_core::{Bytes as SpCoreBytes, H256};
use sp_core_hashing::twox_128;

use std::{
    convert::TryInto,
    env,
    fmt::Debug,
    fs::{self, File},
    io::{copy, Cursor},
    os::unix::prelude::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    str,
    time::Duration,
};

use nix::{
    sys::signal::{self, Signal},
    unistd::Pid,
};

use async_trait::async_trait;

pub const PARACHAIN_MODULE: &str = "VaultRegistry";
pub const CURRENT_RELEASE_STORAGE_ITEM: &str = "CurrentClientRelease";
pub const PENDING_RELEASE_STORAGE_ITEM: &str = "PendingClientRelease";
pub const BLOCK_TIME: Duration = Duration::from_secs(6);

#[derive(Encode, Decode, Default, Eq, PartialEq, Debug)]
pub struct ClientRelease {
    pub uri: String,
    pub code_hash: H256,
}

#[derive(Default, Eq, PartialEq, Debug)]
pub struct DownloadedRelease {
    pub release: ClientRelease,
    pub path: PathBuf,
    pub bin_name: String,
}

#[async_trait]
pub trait VaultvisorUtils {
    async fn query_storage(&self, maybe_storage_key: Option<&str>, method: &str) -> Option<SpCoreBytes>;
    async fn read_chain_storage<T: Decode + Debug>(&self, maybe_storage_key: Option<&str>) -> Result<Option<T>, Error>;
    async fn try_get_release(&self, pending: bool) -> Result<Option<ClientRelease>, Error>;
    async fn download_binary(&mut self, release: ClientRelease) -> Result<(), Error>;
    fn delete_downloaded_release(&mut self) -> Result<(), Error>;
    async fn run_binary(&mut self) -> Result<(), Error>;
    fn terminate_proc_and_wait(&mut self) -> Result<(), Error>;
    async fn get_request_bytes(url: String) -> Result<Bytes, Error>;
    async fn ws_client(url: &str) -> Result<WsClient, Error>;
}

pub struct Vaultvisor {
    parachain_rpc: WsClient,
    vault_args: Vec<String>,
    child_proc: Option<Child>,
    downloaded_release: Option<DownloadedRelease>,
    download_path: PathBuf,
}

impl Vaultvisor {
    pub fn new(parachain_rpc: WsClient, vault_args: Vec<String>, download_path: PathBuf) -> Self {
        Self {
            parachain_rpc,
            vault_args,
            child_proc: None,
            downloaded_release: None,
            download_path,
        }
    }

    pub async fn run(&mut self) -> Result<(), Error> {
        let release = self.try_get_release(false).await?.expect("No current release");
        // WARNING: This will overwrite any pre-existing binary with the same name
        self.download_binary(release).await?;

        self.run_binary().await?;
        loop {
            if let Some(new_release) = self.try_get_release(false).await? {
                let downloaded_release = self.downloaded_release.as_ref().ok_or(Error::NoDownloadedRelease)?;
                if new_release.uri != downloaded_release.release.uri {
                    // Wait for child process to finish completely.
                    // To ensure there can't be two vault processes using the same Bitcoin wallet.
                    self.terminate_proc_and_wait()?;

                    // Delete old release
                    self.delete_downloaded_release()?;

                    // Download new release
                    self.download_binary(new_release).await?;

                    // Run the downloaded release
                    self.run_binary().await?;
                }
            }
            tokio::time::sleep(BLOCK_TIME).await;
        }
    }
}

#[async_trait]
impl VaultvisorUtils for Vaultvisor {
    async fn query_storage(&self, maybe_storage_key: Option<&str>, method: &str) -> Option<SpCoreBytes> {
        let params = maybe_storage_key.map_or(rpc_params![], |key| rpc_params![key]);
        self.parachain_rpc.request(method, params).await.ok()
    }

    async fn read_chain_storage<T: Decode + Debug>(&self, maybe_storage_key: Option<&str>) -> Result<Option<T>, Error> {
        let enc_res = self.query_storage(maybe_storage_key, "state_getStorage").await;
        enc_res
            .map(|r| {
                let v = r.to_vec();
                T::decode(&mut &v[..])
            })
            .transpose()
            .map_err(Into::into)
    }

    async fn try_get_release(&self, pending: bool) -> Result<Option<ClientRelease>, Error> {
        let storage_item = if pending {
            PENDING_RELEASE_STORAGE_ITEM
        } else {
            CURRENT_RELEASE_STORAGE_ITEM
        };
        let storage_key = compute_storage_key(PARACHAIN_MODULE.to_string(), storage_item.to_string());
        Ok(self
            .read_chain_storage::<ClientRelease>(Some(storage_key.as_str()))
            .await?)
    }

    async fn run_binary(&mut self) -> Result<(), Error> {
        // Ensure there is no other child running
        if self.child_proc.is_some() {
            return Err(Error::ChildProcessExists);
        }
        let downloaded_release = self.downloaded_release.as_ref().ok_or(Error::NoDownloadedRelease)?;
        let mut child = Command::new(format!("./{}", downloaded_release.bin_name))
            .args(self.vault_args.clone())
            .stdout(Stdio::inherit())
            .spawn()?;
        self.child_proc = Some(child);
        Ok(())
    }

    async fn download_binary(&mut self, release: ClientRelease) -> Result<(), Error> {
        // Remove any trailing slashes from the release URI
        let parsed_uri = Url::parse(&release.uri.trim_end_matches("/"))?;
        let bin_name = parsed_uri
            .path_segments()
            .and_then(|segments| segments.last())
            .and_then(|name| if name.is_empty() { None } else { Some(name) })
            .ok_or(Error::ClientNameDerivationError)?;
        let bin_path = self.download_path.join(bin_name);
        log::info!("Downloading {} at: {:?}", bin_name, bin_path);
        let mut bin_file = File::create(bin_path.clone())?;

        let bytes = Self::get_request_bytes(release.uri.clone()).await?;
        let mut content = Cursor::new(bytes);

        copy(&mut content, &mut bin_file)?;

        // Make the binary executable.
        // The set permissions are: -rwx------
        fs::set_permissions(bin_path.clone(), fs::Permissions::from_mode(0o700))?;

        self.downloaded_release = Some(DownloadedRelease {
            release,
            path: bin_path,
            bin_name: bin_name.to_string(),
        });
        Ok(())
    }

    fn delete_downloaded_release(&mut self) -> Result<(), Error> {
        let release = self.downloaded_release.as_ref().ok_or(Error::NoDownloadedRelease)?;
        log::info!("Removing old release, with path {:?}", release.path);
        fs::remove_file(release.path.clone())?;
        self.downloaded_release = None;
        Ok(())
    }

    fn terminate_proc_and_wait(&mut self) -> Result<(), Error> {
        let child_proc = self.child_proc.as_mut().ok_or(Error::NoChildProcess)?;
        signal::kill(
            Pid::from_raw(child_proc.id().try_into().map_err(|_| Error::IntegerConversionError)?),
            Signal::SIGTERM,
        )?;

        match child_proc.wait() {
            Ok(exit_code) => log::info!("Outdated vault killed with exit code {}", exit_code),
            Err(error) => log::warn!("Outdated vault shutdown error: {}", error),
        };
        self.child_proc = None;
        Ok(())
    }

    async fn get_request_bytes(url: String) -> Result<Bytes, Error> {
        let response = reqwest::get(url.clone()).await?;
        Ok(response.bytes().await?)
    }

    async fn ws_client(url: &str) -> Result<WsClient, Error> {
        Ok(WsClientBuilder::default().build(url).await?)
    }
}

fn compute_storage_key(module: String, key: String) -> String {
    let module = twox_128(module.as_bytes());
    let item = twox_128(key.as_bytes());
    let key = hex::encode([module, item].concat());
    format!("0x{}", key)
}
