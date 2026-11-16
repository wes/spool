//! Asks every model the ChatGPT sign-in offers for one short reply.
//!
//! The writing tools build their requests from the backend's model list, and
//! each model can want them sent differently. This settles whether every
//! model Settings offers actually answers, without clicking through each one
//! in a composer. Uses the sign-in from Settings → Writing, and renews it the
//! way the app does.
//!
//!     cargo run -p spool-net --example chatgpt_probe            # every model
//!     cargo run -p spool-net --example chatgpt_probe -- gpt-5.5 # just one

use spool_net::chatgpt;

fn main() -> anyhow::Result<()> {
    smol::block_on(async {
        let only = std::env::args().nth(1);
        let offered = chatgpt::models().await?;
        println!("default: {}", offered.default.as_deref().unwrap_or("(none)"));

        let mut failed = 0;
        for model in &offered.models {
            if only.as_ref().is_some_and(|only| *only != model.slug) {
                continue;
            }
            chatgpt::set_model(Some(model.slug.clone()));
            match chatgpt::respond("Reply with the single word OK.", "Are you there?").await {
                Ok(reply) => println!("ok    {}  {:?}", model.slug, reply.trim()),
                Err(e) => {
                    failed += 1;
                    println!("FAIL  {}  {e:#}", model.slug);
                }
            }
        }
        match failed {
            0 => Ok(()),
            n => anyhow::bail!("{n} model(s) refused"),
        }
    })
}
