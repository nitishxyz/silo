//! Time each step of opening a block on a remote host, over one live
//! connection (as the app has): `cargo run -p silo-cli --example open_timing -- box`.
//! Makes a throwaway session there and closes it.

use std::time::Instant;

use silo_client::DataStream;
use silo_proto::control::{ClientIds, Created, Method, SpawnSpec};
use silo_proto::model::{Axis, BlockKind};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let host = std::env::args().nth(1).unwrap_or_else(|| "box".into());
    let home = silo_proto::paths::home();
    let t = Instant::now();
    let remote = silo_cli::connect_remote(&home, &host, &Default::default(), "open-timing").await?;
    let control = remote.control;
    println!("connect          {:>6} ms", t.elapsed().as_millis());
    let ms = |t: Instant| t.elapsed().as_millis();
    for _ in 0..3 {
        let t = Instant::now();
        control.call_value(Method::Ping {}).await?;
        println!("ping             {:>6} ms", ms(t));
    }
    let t = Instant::now();
    let created: Created = control
        .call(Method::SessionCreate { name: Some("open-timing".into()), spawn: SpawnSpec::default(), kind: BlockKind::Terminal, url: None })
        .await?;
    println!("session.create   {:>6} ms", ms(t));
    let supports = control.peer_supports(silo_proto::compat::feature::INSTANT_BLOCKS);
    println!("instant_blocks   {supports}");
    for i in 0..3 {
        let t = Instant::now();
        let ids = supports.then(|| ClientIds { block_id: format!("b{:08x}", 0xabc0_0000u32 + i), window_id: None });
        let new: Created = control
            .call(Method::BlockSplit {
                block_id: created.block_id.clone(),
                axis: Axis::Horizontal,
                spawn: SpawnSpec::default(),
                kind: BlockKind::Terminal,
                url: None,
                agent: None,
                ids,
            })
            .await?;
        println!("block.split      {:>6} ms (ticket: {})", ms(t), new.ticket.is_some());
        let t = Instant::now();
        let stream = match new.ticket {
            Some(ticket) => DataStream::open_ticket(&control, ticket.ticket, None).await?,
            None => DataStream::open(&control, &new.block_id, None).await?,
        };
        println!("attach (welcome) {:>6} ms", ms(t));
        drop(stream);
        let t = Instant::now();
        control.call_value(Method::BlocksBusy { session_id: None, window_id: None, block_ids: Some(vec![new.block_id.clone()]) }).await?;
        println!("blocks.busy      {:>6} ms", ms(t));
        let t = Instant::now();
        control.call_value(Method::BlockClose { block_id: new.block_id, force: false }).await?;
        println!("block.close      {:>6} ms", ms(t));
    }
    control.call_value(Method::SessionClose { session_id: created.session_id, force: true }).await?;
    Ok(())
}
