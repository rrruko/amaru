use amaru_examples_shared::{forward_ledger, RAW_BLOCK_CONWAY_3};

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn ledger() {
    forward_ledger(RAW_BLOCK_CONWAY_3);
}
