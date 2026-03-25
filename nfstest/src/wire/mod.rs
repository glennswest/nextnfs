pub mod rpc;
pub mod xdr;
pub mod nfs3;
pub mod nfs4;

use crate::harness::TestDef;

/// Return the full test registry.
pub fn registry() -> Vec<TestDef> {
    let mut tests = Vec::new();
    tests.extend(nfs3::tests());
    tests.extend(nfs4::v40_tests());
    tests.extend(nfs4::v41_tests());
    tests.extend(nfs4::v42_tests());
    tests
}

/// Dispatch a test by ID to the appropriate executor.
pub async fn execute(test_id: &str, client: &rpc::RpcClient) -> anyhow::Result<()> {
    // Route by prefix
    if test_id.starts_with("W3-") {
        nfs3::execute(test_id, client).await
    } else if test_id.starts_with("W40-") {
        nfs4::execute_v40(test_id, client).await
    } else if test_id.starts_with("W41-") {
        nfs4::execute_v41(test_id, client).await
    } else if test_id.starts_with("W42-") {
        nfs4::execute_v42(test_id, client).await
    } else {
        anyhow::bail!("SKIP: Test {} not yet implemented", test_id)
    }
}
