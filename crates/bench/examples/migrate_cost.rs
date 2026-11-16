//! Scratch: what migration 12 costs on an existing mailbox.
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let path = std::env::temp_dir().join("spool-startup-probe.db");
    let conn = rusqlite::Connection::open(&path)?;
    let rows: i64 = conn.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))?;
    println!("{rows} messages");

    conn.execute_batch(
        "DROP INDEX IF EXISTS idx_messages_threading;
         DROP INDEX IF EXISTS idx_messages_mailbox_flags;
         CREATE INDEX IF NOT EXISTS idx_messages_rfc_id ON messages(rfc_message_id);",
    )?;

    let t = Instant::now();
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_messages_threading
             ON messages(rfc_message_id, norm_subject, thread_id);
         DROP INDEX IF EXISTS idx_messages_rfc_id;
         CREATE INDEX IF NOT EXISTS idx_messages_mailbox_flags
             ON messages(mailbox_id, flags);",
    )?;
    println!("migration 12 built both indexes in {:?}", t.elapsed());

    for (label, sql) in [
        ("threader seed", "SELECT rfc_message_id, norm_subject, thread_id FROM messages"),
        (
            "folder counts",
            "SELECT m.id, COUNT(msg.id), COALESCE(SUM(CASE WHEN msg.flags & 1 = 0 THEN 1 ELSE 0 END),0)
             FROM mailboxes m LEFT JOIN messages msg ON msg.mailbox_id = m.id GROUP BY m.id",
        ),
    ] {
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?
            .query_map([], |r| r.get::<_, String>(3))?
            .collect::<Result<_, _>>()?;
        println!("{label}: {}", plan.join(" | "));
    }
    Ok(())
}
