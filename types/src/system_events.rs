// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::transaction::SignedUserTransaction;
use actix::prelude::*;

//TODO this type should at another crate and avoid starcoin-types dependency actix ?.
#[derive(Clone, Debug, Message)]
#[rtype(result = "()")]
pub enum SystemEvents {
    NewUserTransaction(SignedUserTransaction),
}