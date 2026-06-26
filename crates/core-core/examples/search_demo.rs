use std::{
    collections::BTreeMap,
    io::{self, Write},
};

use core_core::{CorelamoDatabase, DatabaseOptions};
use core_storage::search_database::DocumentInput;

fn fixture_path() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../core-testkit/fixtures/dbpedia.jsonl"
    )
}

#[derive(Debug, serde::Deserialize)]
struct InputDoc {
    id: String,
    title: String,
    body: String,
}

fn main() -> io::Result<()> {
    let root = std::env::temp_dir().join("corelamo-search-demo");
    std::fs::remove_dir_all(&root).ok();
    //
    println!("fixture={}", fixture_path());
    println!("exists={}", std::path::Path::new(fixture_path()).exists());

    let mut db = CorelamoDatabase::open(&root, DatabaseOptions::default())?;
    println!("database root={}", db.root().display());
    println!("policy file={}", db.policy_path().display());
    println!("policy={:#?}", db.policy());
    println!("stats={:#?}", db.stats()?);

    if db.stats()?.document_count == 0 {
        let docs = load_jsonl(fixture_path())?;

        db.put_documents_parallel(
            docs.into_iter()
                .map(|doc| DocumentInput {
                    external_id: doc.id,
                    fields: BTreeMap::from([
                        ("title".to_string(), doc.title),
                        ("body".to_string(), doc.body),
                    ]),
                })
                .collect(),
        )?;
    } else {
        println!("documents already loaded; skipping import");
    }

    loop {
        print!("\nquery> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        let raw = input.trim();

        if raw.is_empty() || raw == "exit" {
            break;
        }

        if raw == ":stats" {
            println!("{:#?}", db.stats()?);
            continue;
        }

        if raw == ":policy" {
            println!("{:#?}", db.policy());
            continue;
        }

        if raw == ":policy-path" {
            println!("{}", db.policy_path().display());
            continue;
        }

        if raw == ":reload-policy" {
            db.reload_policy()?;
            println!("policy reloaded");
            continue;
        }

        if raw == ":reindex" {
            db.reindex()?;
            println!("reindex complete");
            continue;
        }

        let started = std::time::Instant::now();
        let hits = db.search(raw, 3)?;
        let elapsed = started.elapsed();

        println!("search took {:?}, hits={}", elapsed, hits.len());

        for hit in hits {
            let title = hit
                .fields
                .get("title")
                .map(String::as_str)
                .unwrap_or("<no title>");

            let body = hit
                .fields
                .get("body")
                .map(String::as_str)
                .unwrap_or("<no body>");

            println!(
                "\n{}\nscore={:.2}\n\ntitle:\n{}\n\nbody:\n{}\n",
                hit.external_id, hit.score, title, body,
            );
        }
    }

    db.shutdown()?;

    Ok(())
}

fn load_jsonl(path: &str) -> io::Result<Vec<InputDoc>> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);

    let mut docs = Vec::new();

    for line in std::io::BufRead::lines(reader) {
        let line = line?;

        if line.trim().is_empty() {
            continue;
        }

        let doc: InputDoc = serde_json::from_str(&line)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        docs.push(doc);
    }

    Ok(docs)
}
