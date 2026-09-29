use caskdb::CaskDb;
use std::io::{self, BufRead, Write};

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Put(Vec<u8>, Vec<u8>),
    Get(Vec<u8>),
    Delete(Vec<u8>),
    Compact,
    Exit,
    Unknown(String),
}

/// Parses one REPL line. Pure and unit-tested on its own; the REPL loop
/// itself is thin I/O glue around this.
fn parse_command(line: &str) -> Command {
    let mut parts = line.trim().splitn(3, ' ');
    match parts.next().unwrap_or("") {
        "put" => match (parts.next(), parts.next()) {
            (Some(key), Some(value)) => {
                Command::Put(key.as_bytes().to_vec(), value.as_bytes().to_vec())
            }
            _ => Command::Unknown(line.to_string()),
        },
        "get" => match parts.next() {
            Some(key) => Command::Get(key.as_bytes().to_vec()),
            None => Command::Unknown(line.to_string()),
        },
        "delete" => match parts.next() {
            Some(key) => Command::Delete(key.as_bytes().to_vec()),
            None => Command::Unknown(line.to_string()),
        },
        "compact" => Command::Compact,
        "exit" => Command::Exit,
        _ => Command::Unknown(line.to_string()),
    }
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "./caskdb-data".to_string());
    let mut db = CaskDb::open(&dir).expect("failed to open database");
    println!("caskdb REPL — data dir: {dir}");
    println!("commands: put <key> <value> | get <key> | delete <key> | compact | exit");

    let stdin = io::stdin();
    loop {
        print!("> ");
        io::stdout().flush().unwrap();

        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap() == 0 {
            break; // EOF
        }

        match parse_command(&line) {
            Command::Put(key, value) => match db.put(&key, &value) {
                Ok(()) => println!("OK"),
                Err(e) => println!("ERR {e}"),
            },
            Command::Get(key) => match db.get(&key) {
                Ok(Some(value)) => println!("{}", String::from_utf8_lossy(&value)),
                Ok(None) => println!("(nil)"),
                Err(e) => println!("ERR {e}"),
            },
            Command::Delete(key) => match db.delete(&key) {
                Ok(()) => println!("OK"),
                Err(e) => println!("ERR {e}"),
            },
            Command::Compact => match db.compact() {
                Ok(()) => println!("OK"),
                Err(e) => println!("ERR {e}"),
            },
            Command::Exit => break,
            Command::Unknown(line) => println!("unrecognized command: {line}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_put_get_delete_compact_exit() {
        assert_eq!(
            parse_command("put mykey myvalue"),
            Command::Put(b"mykey".to_vec(), b"myvalue".to_vec())
        );
        assert_eq!(parse_command("get mykey"), Command::Get(b"mykey".to_vec()));
        assert_eq!(
            parse_command("delete mykey"),
            Command::Delete(b"mykey".to_vec())
        );
        assert_eq!(parse_command("compact"), Command::Compact);
        assert_eq!(parse_command("exit"), Command::Exit);
    }

    #[test]
    fn put_value_may_contain_spaces() {
        assert_eq!(
            parse_command("put mykey hello world"),
            Command::Put(b"mykey".to_vec(), b"hello world".to_vec())
        );
    }

    #[test]
    fn missing_arguments_are_unknown() {
        assert_eq!(
            parse_command("put onlykey"),
            Command::Unknown("put onlykey".to_string())
        );
        assert_eq!(
            parse_command("banana"),
            Command::Unknown("banana".to_string())
        );
    }
}
