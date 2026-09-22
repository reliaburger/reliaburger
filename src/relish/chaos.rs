//! Read-only legacy chaos status and explicit migration errors.
//!
//! Destructive scenarios use the guarded catalogue, which owns exact fault IDs.
use super::RelishError;
use super::client::BunClient;

fn retired() -> RelishError {
    RelishError::RetiredCommand {
        command: "chaos mutation".to_owned(),
        replacement: "relish test --chaos (guarded scenarios); for existing faults, use relish fault list and clear only an owned ID with relish fault clear <id> --node <node> --acknowledge".to_owned(),
    }
}

/// Refuse the retired scenario before any network request or mutation.
pub async fn council_partition(
    _client: &BunClient,
    _acknowledged: bool,
) -> Result<(), RelishError> {
    Err(retired())
}

/// Refuse the retired scenario before any network request or mutation.
pub async fn worker_isolation(_client: &BunClient, _acknowledged: bool) -> Result<(), RelishError> {
    Err(retired())
}

/// Show active chaos state.
pub async fn status(client: &BunClient) -> Result<(), RelishError> {
    let state = client.chaos_status().await?;
    match state.active_partition {
        Some(p) => {
            println!("Active partitions:");
            println!(
                "  blocking {} peer(s): {} ({}s remaining)",
                p.peers.len(),
                p.peers.join(", "),
                p.remaining_secs
            );
        }
        None => {
            println!("No active partitions");
        }
    }
    Ok(())
}

/// Refuse blanket cleanup: it cannot establish ownership of other operators' faults.
pub async fn heal(_client: &BunClient) -> Result<(), RelishError> {
    Err(retired())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retired_mutations_refuse_before_contacting_a_node() {
        let client = BunClient::new_with_token("http://127.0.0.1:1", None);
        for acknowledged in [false, true] {
            for result in [
                council_partition(&client, acknowledged).await,
                worker_isolation(&client, acknowledged).await,
                heal(&client).await,
            ] {
                let error = result.unwrap_err().to_string();
                assert!(error.contains("retired"), "{error}");
                assert!(error.contains("relish test --chaos"), "{error}");
            }
        }
    }
}
