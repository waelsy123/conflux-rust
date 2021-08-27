// Copyright 2020 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

use crate::rpc::traits::pos::Pos;
use jsonrpc_core::Result as JsonRpcResult;
use crate::rpc::types::pos::Status;
// use crate::common::delegate_convert::into_jsonrpc_result;
use diemdb::DiemDB;
use std::sync::Arc;
use storage_interface::DbReader;

pub struct PosHandler {
    diem_db: Arc<DiemDB>
}

impl PosHandler {
    pub fn new(diem_db: Arc<DiemDB>) -> Self {
        PosHandler{
            diem_db,
        }
    }

    fn status_impl(&self) -> Status {
        let state = self.diem_db.get_latest_pos_state();
        let decision = state.pivot_decision();
        let epoch_state = state.epoch_state();
        let round = state.current_view();
        Status{
            chain_id: 1,  // TODO find the chain_id
            epoch: epoch_state.epoch,
            block_number: round,
            catch_up_mode: state.catch_up_mode(),
            pivot_decision: decision.clone(),
        }
    }
}

impl Pos for PosHandler {
    fn pos_status(&self) -> JsonRpcResult<Status> {
        let status = self.status_impl();
        Ok(status)
        // into_jsonrpc_result(Ok(status))
    }
}