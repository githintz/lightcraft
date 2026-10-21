//! Sign in to Google Drive and print the refresh token, for the live tests
//! (`tests/live.rs`) and for trying the provider by hand:
//!
//! ```sh
//! cargo run -p lightcraft-cloud --example gdrive-signin -- <client id> [<client secret>]
//! ```
//!
//! The client id comes from a Google Cloud project with the Drive API enabled and an OAuth
//! client of type "Desktop app". Open the printed URL, allow access, and the browser comes back to
//! this program. Access is limited to the files LightCraft creates (`drive.file`).

use std::time::Duration;

use lightcraft_cloud::Net;
use lightcraft_cloud::crypto::OsRandom;
use lightcraft_cloud::oauth::{LoopbackFlow, OAuthClient};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let Some(client_id) = args.next() else {
        eprintln!("usage: gdrive-signin <client id> [<client secret>]");
        std::process::exit(2);
    };
    let client = OAuthClient::google_drive(client_id, args.next());
    let flow = LoopbackFlow::start(client, &mut OsRandom)?;
    println!("Open this URL in your browser and allow access:\n\n{}\n", flow.authorize_url());
    let tokens = flow.wait(&Net::default(), Duration::from_secs(300))?;
    println!("LIGHTCRAFT_TEST_GDRIVE_REFRESH_TOKEN={}", tokens.refresh_token.unwrap_or_default());
    Ok(())
}
