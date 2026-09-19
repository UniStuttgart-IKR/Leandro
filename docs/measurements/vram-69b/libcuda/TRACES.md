<!-- SPDX-License-Identifier: MIT -->
# Trace files

- `mode-on.jsonl`: complete trace, 164 records.
- `mode-off.jsonl`: first 400 records of the original 4474-record trace.
- `analysis.txt` was produced from the complete pair; both stored files include
  the first divergence at record 146.
- Files start with `#` license/provenance comments, followed by one JSON record
  per line. Skip comment lines before parsing. Format: `crates/nvrm-trace/src/log.rs`.
- Both files are licensed MIT, copyright 2026 Silas Müller <github@silasmueller.de>.
