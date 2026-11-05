#[tokio::main]
async fn main() {
    let identity = cord_core::identity::Identity::new(0);
    let client = reqwest::Client::builder().user_agent(identity.user_agent()).build().unwrap();
    match cord_core::build_number::fetch(&client).await {
        Some(n) => println!("{n}"),
        None => { eprintln!("build number not found"); std::process::exit(1) }
    }
}
