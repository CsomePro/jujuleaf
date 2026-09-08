#[tokio::main]
async fn main() {
    if let Err(error) = jujuleaf::cli::run().await {
        if let Some(exit) = error.downcast_ref::<jujuleaf::bridge::ReportedBridgeExit>() {
            std::process::exit(exit.code());
        }
        jujuleaf::output::print_error(&error);
        std::process::exit(1);
    }
}
