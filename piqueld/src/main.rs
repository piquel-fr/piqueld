use tokio;

#[tokio::main]
async fn main() {
    match piqueld::run().await {
        Ok(_) => {}
        Err(err) => panic!("Error: {err:#}"),
    };
}
