//! Non-interactive check that the ScreenCast portal can be reached.
use ashpd::desktop::screencast::{Screencast, SourceType};

fn main() {
    let out = futures_lite::future::block_on(async {
        let proxy = Screencast::new().await?;
        let version = proxy.version();
        let session = proxy.create_session(Default::default()).await?;
        let types = proxy.available_source_types().await?;
        let cursors = proxy.available_cursor_modes().await?;
        Ok::<_, ashpd::Error>((version, types, cursors, session))
    });
    match out {
        Ok((v, t, c, _session)) => println!(
            "portal screencast v{v} source_types={} cursor_modes={}",
            t.bits(),
            c.bits()
        ),
        Err(e) => println!("portal error: {e}"),
    }
    let _ = SourceType::MONITOR;
}
