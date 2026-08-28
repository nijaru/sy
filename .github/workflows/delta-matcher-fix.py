from pathlib import Path

p = Path("src/transfer/delta.rs")
text = p.read_text()
text = text.replace(
    """    if first.is_empty() {\n        return Ok(emitter.finish()?);\n    }\n""",
    """    if first.is_empty() {\n        return emitter.finish();\n    }\n""",
    1,
)
text = text.replace(
    """fn read_byte<R: Read>(reader: &mut BufReader<R>) -> io::Result<Option<u8>> {\n    loop {\n        let available = reader.fill_buf()?;\n        if available.is_empty() {\n            return Ok(None);\n        }\n        let byte = available[0];\n        reader.consume(1);\n        return Ok(Some(byte));\n    }\n}\n""",
    """fn read_byte<R: Read>(reader: &mut BufReader<R>) -> io::Result<Option<u8>> {\n    let available = reader.fill_buf()?;\n    if available.is_empty() {\n        return Ok(None);\n    }\n    let byte = available[0];\n    reader.consume(1);\n    Ok(Some(byte))\n}\n""",
    1,
)
p.write_text(text)
