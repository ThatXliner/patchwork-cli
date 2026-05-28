mod cli;
mod edit;
mod language;
mod matcher;

use std::io::{self, Read, Write};

use clap::Parser;
use cli::{Cli, Command};
use edit::{apply_edits, matches_to_edits, Operation};
use language::Lang;
use matcher::{find_query_matches, find_snippet_matches, Match};

fn emit_claude_code_hint() {
    if std::env::var("CLAUDECODE").is_ok() {
        let _ = writeln!(
            io::stderr(),
            r#"<claude-code-hint v="1" type="plugin" value="patchwork@ThatXliner" />"#
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    emit_claude_code_hint();
    let cli = Cli::parse();

    match &cli.command {
        Command::Find {
            pattern,
            query,
            files,
        } => cmd_find(pattern, query, files, &cli),
        Command::Replace {
            pattern,
            query,
            replacement,
            in_place,
            files,
        } => cmd_edit(
            pattern,
            query,
            files,
            &cli,
            Operation::Replace(replacement.clone()),
            *in_place,
        ),
        Command::Delete {
            pattern,
            query,
            in_place,
            files,
        } => cmd_edit(pattern, query, files, &cli, Operation::Delete, *in_place),
        Command::InsertBefore {
            pattern,
            query,
            code,
            in_place,
            files,
        } => cmd_edit(
            pattern,
            query,
            files,
            &cli,
            Operation::InsertBefore(code.clone()),
            *in_place,
        ),
        Command::InsertAfter {
            pattern,
            query,
            code,
            in_place,
            files,
        } => cmd_edit(
            pattern,
            query,
            files,
            &cli,
            Operation::InsertAfter(code.clone()),
            *in_place,
        ),
        Command::Nodes { language, all } => {
            let lang = Lang::from_name(language).ok_or_else(|| {
                let supported: Vec<&str> = Lang::all().iter().map(|l| l.name()).collect();
                format!("Unknown language '{}'. Supported: {}", language, supported.join(", "))
            })?;
            let kinds = lang.list_node_kinds(*all);
            for kind in kinds {
                println!("{kind}");
            }
            Ok(())
        }
    }
}

fn get_matches(
    source: &str,
    pattern: &Option<String>,
    query: &Option<String>,
    lang: &Lang,
) -> Result<Vec<Match>, String> {
    match (pattern, query) {
        (Some(p), None) => find_snippet_matches(source, p, &lang.grammar()),
        (None, Some(q)) => find_query_matches(source, q, &lang.grammar()),
        _ => unreachable!(),
    }
}

fn resolve_lang(lang_opt: &Option<String>, file: Option<&str>) -> Result<Lang, String> {
    if let Some(name) = lang_opt {
        Lang::from_name(name).ok_or_else(|| {
            let supported: Vec<&str> = Lang::all().iter().map(|l| l.name()).collect();
            format!("Unknown language '{}'. Supported: {}", name, supported.join(", "))
        })
    } else if let Some(path) = file {
        Lang::from_extension(path).ok_or_else(|| {
            format!(
                "Could not detect language from '{}'. Use --language to specify.",
                path
            )
        })
    } else {
        Err("Could not detect language. Use --language when reading from stdin.".to_string())
    }
}

fn read_source(path: Option<&str>) -> io::Result<String> {
    match path {
        Some(p) => std::fs::read_to_string(p),
        None => {
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf)?;
            Ok(buf)
        }
    }
}

fn print_find_results(matches: &[Match], file: Option<&str>, source: &str) {
    let prefix = file.map_or(String::new(), |f| format!("{}:", f));
    for m in matches {
        // Convert byte offsets to line:col (using 1-based line, 0-based column)
        let line = m.start_point.row + 1;
        let col = m.start_point.column;
        // Show matched text snippet
        let text = &source[m.start_byte..m.end_byte];
        let snippet = text.lines().next().unwrap_or(text);
        println!("{}{}:{}: {}", prefix, line, col, snippet);
    }
}

fn process_edit(
    source: &str,
    pattern: &Option<String>,
    query: &Option<String>,
    lang: &Lang,
    op: &Operation,
) -> Result<String, String> {
    let matches = get_matches(source, pattern, query, lang)?;
    if matches.is_empty() {
        return Err("No matches found".to_string());
    }
    let edits = matches_to_edits(&matches, op);
    apply_edits(source, &edits)
}

fn cmd_find(
    pattern: &Option<String>,
    query: &Option<String>,
    files: &[String],
    cli: &Cli,
) -> Result<(), Box<dyn std::error::Error>> {
    cli::ensure_pattern_or_query(pattern, query)?;

    if files.is_empty() {
        let lang = resolve_lang(&cli.language, None)?;
        let source = read_source(None)?;
        let matches = get_matches(&source, pattern, query, &lang)?;
        print_find_results(&matches, None, &source);
        return Ok(());
    }

    for file in files {
        let lang = resolve_lang(&cli.language, Some(file))?;
        let source = read_source(Some(file))?;
        let matches = get_matches(&source, pattern, query, &lang)?;
        let prefix = if files.len() > 1 {
            Some(file.as_str())
        } else {
            None
        };
        print_find_results(&matches, prefix, &source);
    }

    Ok(())
}

fn cmd_edit(
    pattern: &Option<String>,
    query: &Option<String>,
    files: &[String],
    cli: &Cli,
    op: Operation,
    in_place: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    cli::ensure_pattern_or_query(pattern, query)?;

    if files.is_empty() {
        if in_place {
            return Err("Cannot use --in-place without files".into());
        }
        let lang = resolve_lang(&cli.language, None)?;
        let source = read_source(None)?;
        let result = process_edit(&source, pattern, query, &lang, &op)?;
        print!("{}", result);
        return Ok(());
    }

    for file in files {
        let lang = resolve_lang(&cli.language, Some(file))?;
        let source = read_source(Some(file))?;
        match process_edit(&source, pattern, query, &lang, &op) {
            Ok(result) => {
                if in_place {
                    std::fs::write(file, &result)?;
                } else {
                    // For multiple files, prefix with filename
                    if files.len() > 1 {
                        println!("==> {} <==", file);
                    }
                    print!("{}", result);
                    if files.len() > 1 {
                        println!();
                    }
                }
            }
            Err(e) => {
                eprintln!("{}: {}", file, e);
            }
        }
    }

    Ok(())
}
