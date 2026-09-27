//! Terminal rendering of [`insights`](crate::insights).

use anyhow::Result;
use comfy_table::{CellAlignment, ContentArrangement, Table, presets::UTF8_FULL_CONDENSED};
use rusqlite::Connection;

use crate::insights::{self, CostBy, Since};

fn table(headers: &[&str], right_aligned_from: usize) -> Table {
    let mut t = Table::new();
    t.load_style(UTF8_FULL_CONDENSED.with_rounded_corners())
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(headers.to_vec());
    for i in right_aligned_from..headers.len() {
        if let Some(col) = t.column_mut(i) {
            col.set_cell_alignment(CellAlignment::Right);
        }
    }
    t
}

pub fn money(v: f64) -> String {
    if v >= 100.0 {
        format!("${v:.0}")
    } else {
        format!("${v:.2}")
    }
}

pub fn tokens(n: i64) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.1}B", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

fn pct(v: Option<f64>) -> String {
    v.map_or("-".into(), |v| format!("{:.0}%", v * 100.0))
}

fn day(ts: &str) -> &str {
    ts.get(..10).unwrap_or(ts)
}

fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

pub fn summary(conn: &Connection, since: &Since) -> Result<()> {
    let s = insights::summary(conn, since)?;
    let range = match (&s.first_ts, &s.last_ts) {
        (Some(a), Some(b)) => format!("{} → {}", day(a), day(b)),
        _ => "no data yet".into(),
    };
    println!("calvin · {range}\n");
    let mut t = table(&["Measure", "Value"], 1);
    t.add_row(vec!["Sessions".to_string(), s.sessions.to_string()]);
    t.add_row(vec!["Prompts typed".to_string(), s.prompts.to_string()]);
    t.add_row(vec!["Slash commands".to_string(), s.commands.to_string()]);
    t.add_row(vec!["Model requests".to_string(), s.requests.to_string()]);
    t.add_row(vec![
        "Est. cost (API list price)".to_string(),
        money(s.cost_usd),
    ]);
    t.add_row(vec!["Cache hit rate".to_string(), pct(s.cache_hit_rate)]);
    println!("{t}");
    if s.unpriced_requests > 0 {
        println!(
            "{} requests use a model with no known price and are not in the cost.",
            s.unpriced_requests
        );
    }
    println!("\nMore: calvin report cost | skills | commands | prompts | friction | cache");
    Ok(())
}

pub fn cost(conn: &Connection, since: &Since, by: CostBy) -> Result<()> {
    let label = match by {
        CostBy::Day => "Day",
        CostBy::Project => "Project",
        CostBy::Model => "Model",
    };
    let rows = insights::cost(conn, since, by)?;
    let mut t = table(
        &[
            label,
            "Sessions",
            "Requests",
            "Input",
            "Output",
            "Cache read",
            "Cache write",
            "Est. cost",
        ],
        1,
    );
    let mut total = 0.0;
    for r in &rows {
        total += r.cost_usd;
        t.add_row(vec![
            r.key.clone(),
            r.sessions.to_string(),
            r.requests.to_string(),
            tokens(r.input_tokens),
            tokens(r.output_tokens),
            tokens(r.cache_read),
            tokens(r.cache_write),
            money(r.cost_usd),
        ]);
    }
    println!("{t}");
    println!("Total: {} (estimated at API list price)", money(total));
    Ok(())
}

pub fn skills(conn: &Connection, since: &Since) -> Result<()> {
    let rows = insights::skills(conn, since)?;
    if rows.is_empty() {
        println!("No skill runs in this period.");
        return Ok(());
    }
    let mut t = table(&["Skill", "Runs", "Sessions", "Last used"], 1);
    for r in &rows {
        t.add_row(vec![
            r.skill.clone(),
            r.runs.to_string(),
            r.sessions.to_string(),
            day(&r.last_used).to_string(),
        ]);
    }
    println!("{t}");
    println!(
        "Skills the model chose to run. Skills you ran yourself as /commands: calvin report commands"
    );
    Ok(())
}

pub fn commands(conn: &Connection, since: &Since) -> Result<()> {
    let rows = insights::commands(conn, since)?;
    if rows.is_empty() {
        println!("No slash commands in this period.");
        return Ok(());
    }
    let mut t = table(&["Command", "Uses", "Last used"], 1);
    for r in &rows {
        t.add_row(vec![
            r.command.clone(),
            r.uses.to_string(),
            day(&r.last_used).to_string(),
        ]);
    }
    println!("{t}");
    Ok(())
}

pub fn prompts(conn: &Connection, since: &Since, min: i64, limit: usize) -> Result<()> {
    let rows = insights::repeated_prompts(conn, since, min)?;
    if rows.is_empty() {
        println!("No prompt was typed {min} or more times in this period.");
        return Ok(());
    }
    let mut t = table(&["Prompt", "Times", "Sessions", "Last used"], 1);
    for r in rows.iter().take(limit) {
        t.add_row(vec![
            one_line(&r.example, 70),
            r.count.to_string(),
            r.sessions.to_string(),
            day(&r.last_used).to_string(),
        ]);
    }
    println!("{t}");
    println!("Prompts you keep typing are candidates for a skill or a CLAUDE.md rule.");
    Ok(())
}

pub fn friction(conn: &Connection, since: &Since) -> Result<()> {
    let f = insights::friction(conn, since)?;
    let mut t = table(&["What happened", "Count"], 1);
    for r in &f.by_kind {
        let label = match r.kind.as_str() {
            "rejected" => "You rejected a tool call",
            "denied" => "Blocked by a permission rule or classifier",
            "interrupted" => "You interrupted the agent",
            other => other,
        };
        t.add_row(vec![label.to_string(), r.count.to_string()]);
    }
    t.add_row(vec![
        format!("Tool calls that errored (of {})", f.tool_calls),
        f.tool_errors.to_string(),
    ]);
    println!("{t}");
    if !f.by_tool.is_empty() {
        let mut t = table(&["Tool", "Rejected / denied", "Count"], 2);
        for r in &f.by_tool {
            t.add_row(vec![
                r.tool.clone().unwrap_or_default(),
                r.kind.clone(),
                r.count.to_string(),
            ]);
        }
        println!("\n{t}");
    }
    Ok(())
}

pub fn cache(conn: &Connection, since: &Since) -> Result<()> {
    let rows = insights::cache(conn, since)?;
    let mut t = table(
        &[
            "Model",
            "Cache read",
            "Cache write",
            "Uncached input",
            "Hit rate",
        ],
        1,
    );
    for r in &rows {
        t.add_row(vec![
            r.model.clone(),
            tokens(r.cache_read),
            tokens(r.cache_write),
            tokens(r.uncached_input),
            pct(r.hit_rate),
        ]);
    }
    println!("{t}");
    println!(
        "Hit rate = cache reads / all input tokens. Low rates usually mean long gaps between turns."
    );
    Ok(())
}
