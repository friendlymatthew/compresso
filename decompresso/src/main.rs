use std::{env, fs, io, io::Write, path::PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use compresso::{gzip::GZipReader, zlib::ZlibDecoder};

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let path = PathBuf::from(args.next().context("decompresso <file>")?);

    ensure!(args.next().is_none(), "decompresso <file>");

    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| anyhow!("for now, we expect a file extension"))?;

    let input = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;

    let output = match extension.as_str() {
        "gz" | "gzip" => GZipReader::new(&input).read(),
        "zlib" => ZlibDecoder::new(&input).decode(),
        "zip" => todo!("how does a zip cli look like?"),
        foreign => panic!("found foreign file extension {foreign}"),
    }
    .with_context(|| format!("decompressing {}", path.display()))?;

    io::stdout().lock().write_all(&output)?;
    Ok(())
}
