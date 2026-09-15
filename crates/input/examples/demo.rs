//! Input-injection demo.
//!
//! ```text
//! cargo run -p rc-input --example demo            # dry run: prints the plan
//! cargo run -p rc-input --example demo -- --go    # actually injects
//! ```
//!
//! With `--go` you get a 3-second grace period — focus a text field (Notepad,
//! a browser address bar) and the demo will type into it, then wiggle the
//! mouse and left-click.

use std::thread::sleep;
use std::time::Duration;

use anyhow::Result;
use rc_input::Injector;
use rc_protocol::{InputEvent, PointerButton};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let go = std::env::args().any(|a| a == "--go");

    let script = vec![
        InputEvent::Text("Remote Control input test \u{2713}\r\n".to_string()),
        InputEvent::PointerMove { x: 0.5, y: 0.5 },
        InputEvent::PointerMove { x: 0.55, y: 0.5 },
        InputEvent::PointerMove { x: 0.55, y: 0.55 },
        InputEvent::PointerMove { x: 0.5, y: 0.55 },
        InputEvent::PointerMove { x: 0.5, y: 0.5 },
        InputEvent::PointerButton {
            button: PointerButton::Left,
            pressed: true,
            x: 0.5,
            y: 0.5,
        },
        InputEvent::PointerButton {
            button: PointerButton::Left,
            pressed: false,
            x: 0.5,
            y: 0.5,
        },
    ];

    if !go {
        println!("DRY RUN — would inject {} events:", script.len());
        for e in &script {
            println!("  {e:?}");
        }
        println!("\nre-run with `-- --go` to actually inject.");
        return Ok(());
    }

    println!("injecting in 3s — focus a text field now…");
    sleep(Duration::from_secs(3));

    let mut inj = Injector::new();
    for e in &script {
        println!("-> {e:?}");
        inj.inject(e)?;
        sleep(Duration::from_millis(120));
    }
    inj.release_all();
    println!("done.");
    Ok(())
}
