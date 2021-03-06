// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::cli_state::CliState;
use crate::StarcoinOpt;
use anyhow::{bail, Result};
use scmd::{CommandAction, ExecContext};
use starcoin_crypto::hash::{HashValue, PlainCryptoHash};
use starcoin_executor::executor::Executor;
use starcoin_rpc_client::RemoteStateReader;
use starcoin_state_api::AccountStateReader;
use starcoin_types::account_address::AccountAddress;
use starcoin_vm_types::{language_storage::TypeTag, parser::parse_type_tag};
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(name = "accept_coin")]
pub struct AcceptCoinOpt {
    #[structopt(short = "s")]
    /// if `sender` is absent, use default account.
    sender: Option<AccountAddress>,

    #[structopt(
        short = "g",
        name = "max-gas-amount",
        default_value = "1000000",
        help = "max gas used to deploy the module"
    )]
    max_gas_amount: u64,
    #[structopt(
        short = "p",
        long = "gas-price",
        name = "price of gas",
        default_value = "1",
        help = "gas price used to deploy the module"
    )]
    gas_price: u64,

    #[structopt(
    name = "coin_type",
    help = "coin's type tag, for example: 0x0::STC::T, default is STC",
    parse(try_from_str = parse_type_tag)
    )]
    coin_type: TypeTag,

    #[structopt(
        short = "b",
        name = "blocking-mode",
        long = "blocking",
        help = "blocking wait txn mined"
    )]
    blocking: bool,
}

pub struct AcceptCoinCommand;

impl CommandAction for AcceptCoinCommand {
    type State = CliState;
    type GlobalOpt = StarcoinOpt;
    type Opt = AcceptCoinOpt;
    type ReturnItem = HashValue;

    fn run(
        &self,
        ctx: &ExecContext<Self::State, Self::GlobalOpt, Self::Opt>,
    ) -> Result<Self::ReturnItem> {
        let opt = ctx.opt();
        let client = ctx.state().client();

        let sender = ctx.state().wallet_account_or_default(opt.sender.clone())?;
        let chain_state_reader = RemoteStateReader::new(client);
        let account_state_reader = AccountStateReader::new(&chain_state_reader);
        let account_resource = account_state_reader.get_account_resource(&sender.address)?;

        if account_resource.is_none() {
            bail!(
                "account of module address {} not exists on chain",
                sender.address
            );
        }

        let account_resource = account_resource.unwrap();

        let accept_coin_txn = Executor::build_accept_coin_txn(
            sender.address,
            account_resource.sequence_number(),
            opt.gas_price,
            opt.max_gas_amount,
            opt.coin_type.clone(),
        );

        let signed_txn = client.wallet_sign_txn(accept_coin_txn)?;
        let txn_hash = signed_txn.crypto_hash();
        let succ = client.submit_transaction(signed_txn)?;
        if !succ {
            bail!("execute-txn is reject by node")
        }
        println!("txn {:#x} submitted.", txn_hash);

        if opt.blocking {
            ctx.state().watch_txn(txn_hash)?;
        }

        Ok(txn_hash)
    }
}
