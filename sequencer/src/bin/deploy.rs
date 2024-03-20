use anyhow::{ensure, Context};
use async_compatibility_layer::logging::{setup_backtrace, setup_logging};
use async_std::sync::Arc;
use clap::{builder::OsStr, Parser};
use contract_bindings::{
    erc1967_proxy::ERC1967Proxy,
    hot_shot::HotShot,
    light_client::{LightClient, LIGHTCLIENT_ABI},
    light_client_state_update_vk::LightClientStateUpdateVK,
    plonk_verifier::PlonkVerifier,
};
use derive_more::Display;
use ethers::{
    contract::Contract as ContractBindings,
    prelude::{coins_bip39::English, *},
    solc::artifacts::BytecodeObject,
};
use futures::future::{BoxFuture, FutureExt};
use hotshot_state_prover::service::light_client_genesis;
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::File,
    io::{stdout, BufReader, Write},
    ops::Deref,
    path::{Path, PathBuf},
};
use url::Url;

/// Deploy contracts needed to run the sequencer.
///
/// This script deploys contracts needed to run the sequencer to an L1. It outputs a .env file
/// containing the addresses of the deployed contracts.
///
/// This script can also be used to do incremental deployments. The only contract addresses needed
/// to configure the sequencer network are ESPRESSO_SEQUENCER_HOTSHOT_ADDRESS and
/// ESPRESSO_SEQUENCER_LIGHT_CLIENT_PROXY_ADDRESS. These contracts, however, have dependencies, and
/// a full deployment may involve up to 5 total contracts. Some of these contracts, especially
/// libraries may already have been deployed, or perhaps one of the top-level contracts has been
/// deployed and we only need to deploy the other one.
///
/// It is possible to pass in the addresses of already deployed contracts, in which case those
/// addresses will be used in place of deploying a new contract wherever that contract is required
/// in the deployment process. The generated .env file will include all the addresses passed in as
/// well as those newly deployed.
#[derive(Clone, Debug, Parser)]
struct Options {
    /// A JSON-RPC endpoint for the L1 to deploy to.
    #[clap(
        short,
        long,
        env = "ESPRESSO_SEQUENCER_L1_PROVIDER",
        default_value = "http://localhost:8545"
    )]
    rpc_url: Url,

    /// URL of the HotShot orchestrator.
    ///
    /// This is used to get the stake table for initializing the light client contract.
    #[clap(
        long,
        env = "ESPRESSO_SEQUENCER_ORCHESTRATOR_URL",
        default_value = "http://localhost:40001"
    )]
    orchestrator_url: Url,

    /// Mnemonic for an L1 wallet.
    ///
    /// This wallet is used to deploy the contracts, so the account indicated by ACCOUNT_INDEX must
    /// be funded with with ETH.
    #[clap(
        long,
        name = "MNEMONIC",
        env = "ESPRESSO_SEQUENCER_ETH_MNEMONIC",
        default_value = "test test test test test test test test test test test junk"
    )]
    mnemonic: String,

    /// Account index in the L1 wallet generated by MNEMONIC to use when deploying the contracts.
    #[clap(
        long,
        name = "ACCOUNT_INDEX",
        env = "ESPRESSO_DEPLOYER_ACCOUNT_INDEX",
        default_value = "0"
    )]
    account_index: u32,

    /// Write deployment results to OUT as a .env file.
    ///
    /// If not provided, the results will be written to stdout.
    #[clap(short, long, name = "OUT", env = "ESPRESSO_DEPLOYER_OUT_PATH")]
    out: Option<PathBuf>,

    #[clap(flatten)]
    contracts: DeployedContracts,
}

/// Set of predeployed contracts.
#[derive(Clone, Debug, Parser)]
struct DeployedContracts {
    /// Use an already-deployed HotShot.sol instead of deploying a new one.
    #[clap(long, env = Contract::HotShot)]
    hotshot: Option<Address>,

    /// Use an already-deployed PlonkVerifier.sol instead of deploying a new one.
    #[clap(long, env = Contract::PlonkVerifier)]
    plonk_verifier: Option<Address>,

    /// Use an already-deployed LightClientStateUpdateVK.sol instead of deploying a new one.
    #[clap(long, env = Contract::StateUpdateVK)]
    light_client_state_update_vk: Option<Address>,

    /// Use an already-deployed LightClient.sol instead of deploying a new one.
    #[clap(long, env = Contract::LightClient)]
    light_client: Option<Address>,

    /// Use an already-deployed LightClient.sol proxy instead of deploying a new one.
    #[clap(long, env = Contract::LightClientProxy)]
    light_client_proxy: Option<Address>,
}

/// An identifier for a particular contract.
#[derive(Clone, Copy, Debug, Display, PartialEq, Eq, Hash)]
enum Contract {
    #[display(fmt = "ESPRESSO_SEQUENCER_HOTSHOT_ADDRESS")]
    HotShot,
    #[display(fmt = "ESPRESSO_SEQUENCER_PLONK_VERIFIER_ADDRESS")]
    PlonkVerifier,
    #[display(fmt = "ESPRESSO_SEQUENCER_LIGHT_CLIENT_STATE_UPDATE_VK_ADDRESS")]
    StateUpdateVK,
    #[display(fmt = "ESPRESSO_SEQUENCER_LIGHT_CLIENT_ADDRESS")]
    LightClient,
    #[display(fmt = "ESPRESSO_SEQUENCER_LIGHT_CLIENT_PROXY_ADDRESS")]
    LightClientProxy,
}

impl From<Contract> for OsStr {
    fn from(c: Contract) -> OsStr {
        c.to_string().into()
    }
}

/// Cache of contracts predeployed or deployed during this current run.
struct Contracts(HashMap<Contract, Address>);

impl From<DeployedContracts> for Contracts {
    fn from(deployed: DeployedContracts) -> Self {
        let mut m = HashMap::new();
        if let Some(addr) = deployed.hotshot {
            m.insert(Contract::HotShot, addr);
        }
        if let Some(addr) = deployed.plonk_verifier {
            m.insert(Contract::PlonkVerifier, addr);
        }
        if let Some(addr) = deployed.light_client_state_update_vk {
            m.insert(Contract::StateUpdateVK, addr);
        }
        if let Some(addr) = deployed.light_client {
            m.insert(Contract::LightClient, addr);
        }
        if let Some(addr) = deployed.light_client_proxy {
            m.insert(Contract::LightClientProxy, addr);
        }
        Self(m)
    }
}

impl Contracts {
    /// Deploy a contract by calling a function.
    ///
    /// The `deploy` function will be called only if contract `name` is not already deployed;
    /// otherwise this function will just return the predeployed address. The `deploy` function may
    /// access this [`Contracts`] object, so this can be used to deploy contracts recursively in
    /// dependency order.
    async fn deploy_fn(
        &mut self,
        name: Contract,
        deploy: impl FnOnce(&mut Self) -> BoxFuture<'_, anyhow::Result<Address>>,
    ) -> anyhow::Result<Address> {
        if let Some(addr) = self.0.get(&name) {
            tracing::info!("skipping deployment of {name}, already deployed at {addr:#x}");
            return Ok(*addr);
        }
        tracing::info!("deploying {name}");
        let addr = deploy(self).await?;
        tracing::info!("deployed {name} at {addr:#x}");

        self.0.insert(name, addr);
        Ok(addr)
    }

    /// Deploy a contract by executing its deploy transaction.
    ///
    /// The transaction will only be broadcast if contract `name` is not already deployed.
    async fn deploy_tx<M, C>(
        &mut self,
        name: Contract,
        tx: ContractDeployer<M, C>,
    ) -> anyhow::Result<Address>
    where
        M: Middleware + 'static,
        C: Deref<Target = ContractBindings<M>> + From<ContractInstance<Arc<M>, M>> + Send + 'static,
    {
        self.deploy_fn(name, |_| {
            async {
                let contract = tx.send().await?;
                Ok(contract.address())
            }
            .boxed()
        })
        .await
    }

    /// Write a .env file.
    fn write(&self, mut w: impl Write) -> anyhow::Result<()> {
        for (contract, address) in &self.0 {
            writeln!(w, "{contract}={address:#x}")?;
        }
        Ok(())
    }
}

#[async_std::main]
async fn main() -> anyhow::Result<()> {
    setup_logging();
    setup_backtrace();

    let opt = Options::parse();
    let mut contracts = Contracts::from(opt.contracts);

    let provider = Provider::<Http>::try_from(opt.rpc_url.to_string())?;
    let chain_id = provider.get_chainid().await?.as_u64();
    let wallet = MnemonicBuilder::<English>::default()
        .phrase(opt.mnemonic.as_str())
        .index(opt.account_index)?
        .build()?
        .with_chain_id(chain_id);
    let l1 = Arc::new(SignerMiddleware::new(provider, wallet));

    contracts
        .deploy_tx(Contract::HotShot, HotShot::deploy(l1.clone(), ())?)
        .await?;
    contracts
        .deploy_fn(Contract::LightClientProxy, |contracts| {
            let l1 = l1.clone();
            let orchestrator_url = opt.orchestrator_url.clone();
            async move {
                let light_client = LightClient::new(
                    contracts
                        .deploy_fn(Contract::LightClient, |contracts| {
                            deploy_light_client_contract(l1.clone(), contracts).boxed()
                        })
                        .await?,
                    l1.clone(),
                );
                let genesis = light_client_genesis(&orchestrator_url).await?;
                let data = light_client
                    .initialize(genesis.into(), u32::MAX)
                    .calldata()
                    .context("calldata for initialize transaction not available")?;
                let proxy = ERC1967Proxy::deploy(l1, (light_client.address(), data))?
                    .send()
                    .await?;
                Ok(proxy.address())
            }
            .boxed()
        })
        .await?;

    if let Some(out) = &opt.out {
        let file = File::options().create(true).write(true).open(out)?;
        contracts.write(file)?;
    } else {
        contracts.write(stdout())?;
    }

    Ok(())
}

async fn deploy_light_client_contract<M: Middleware + 'static>(
    l1: Arc<M>,
    contracts: &mut Contracts,
) -> anyhow::Result<Address> {
    // Deploy library contracts.
    let plonk_verifier = contracts
        .deploy_tx(
            Contract::PlonkVerifier,
            PlonkVerifier::deploy(l1.clone(), ())?,
        )
        .await?;
    let vk = contracts
        .deploy_tx(
            Contract::StateUpdateVK,
            LightClientStateUpdateVK::deploy(l1.clone(), ())?,
        )
        .await?;

    // Link with LightClient's bytecode artifacts
    let bytecode_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../contracts/out/LightClient.sol/LightClient.json");
    let json: Value = serde_json::from_reader(BufReader::new(File::open(bytecode_path)?))?;

    let mut bytecode =
        serde_json::from_value::<BytecodeObject>(json["bytecode"]["object"].clone())?;
    bytecode
        .link_fully_qualified(
            "contracts/src/libraries/PlonkVerifier.sol:PlonkVerifier",
            plonk_verifier,
        )
        .resolve()
        .context("error linking PlonkVerifier lib")?;
    bytecode
        .link_fully_qualified(
            "contracts/src/libraries/LightClientStateUpdateVK.sol:LightClientStateUpdateVK",
            vk,
        )
        .resolve()
        .context("error linking LightClientStateUpdateVK lib")?;
    ensure!(!bytecode.is_unlinked(), "failed to link LightClient.sol");

    // Deploy light client.
    let light_client_factory = ContractFactory::new(
        LIGHTCLIENT_ABI.clone(),
        bytecode
            .as_bytes()
            .context("error parsing bytecode for linked LightClient contract")?
            .clone(),
        l1,
    );
    let contract = light_client_factory.deploy(())?.send().await?;
    Ok(contract.address())
}
