#[tokio::main]
async fn main() {
    if let Err(error) = jujuleaf::cli::run().await {
        jujuleaf::cli::print_json_error(&error);
        std::process::exit(1);
    }
}
