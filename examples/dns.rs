use std::io::Result;

use ground::dns::AddressFamily;

async fn async_main() -> Result<()> {
    let mut buffer = [0u8; 512];

    let records = ground::dns::query(
        b"example.org",
        AddressFamily::IPv6,
        "127.0.0.53:53".parse().unwrap(),
        &mut buffer,
    )
    .await?;

    for record in records {
        let record = record?;
        println!(
            "example.org has IPv6 address {} with a TTL of {}",
            record.address, record.ttl
        );
    }

    let result = ground::dns::query(
        b"no.example.org",
        AddressFamily::IPv6,
        "127.0.0.53:53".parse().unwrap(),
        &mut buffer,
    )
    .await;

    println!("no.example.org: {}", result.unwrap_err());

    Ok(())
}

fn main() -> Result<()> {
    executor::run(|_| async_main())
}
