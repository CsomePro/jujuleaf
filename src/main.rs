#[tokio::main]
async fn main() {
    if let Err(error) = jujuleaf::cli::run().await {
        jujuleaf::output::print_error(&error);
        std::process::exit(1);
    }
}
