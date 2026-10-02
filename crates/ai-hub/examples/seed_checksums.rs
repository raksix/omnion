//! Print the seed checksums for `0155_ai_skills.sql`.
//!
//! Run with `cargo run -p omnion-ai-hub --example seed_checksums`, or just read the output and
//! paste it into the migration. The point of the script is that the three values in the
//! migration are *computed*, never typed: a hand-written 64 hex characters is a value nobody
//! can check, and the runtime refuses a skill whose checksum does not describe its own body —
//! so a typo here would make the entire built-in seed unusable and the refusal would be
//! reported as "checksum does not match" on three rows that look perfectly well-formed.

use omnion_ai_hub::skills::checksum_of;

fn main() {
    let seeds: &[(&str, &str, &str, &str, &str)] = &[
        (
            "summary",
            "Summarise",
            "Condense a long document or transcript into the points that matter.",
            "Use when the user asks for a summary, a digest, or \"the short version\" of something long.",
            r#"Thread of intent: the final answer names what this text is about, not what it says.

Method:
1. Read the whole input before writing anything. A summary of the first half is a
   summary of a different document.
2. Keep the claims that carry decisions, numbers, names and dates. Drop the connective
   tissue — transitions, restatements and throat-clearing.
3. Preserve disagreement: if the source contradicts itself, say so rather than picking a side.
4. Mark anything the source asserts without support as an unverified claim.

Length: aim for one tenth of the input, and never return more than the source contains."#,
        ),
        (
            "citation",
            "Cite sources",
            "Ground every factual claim in a numbered source, or say that none exists.",
            "Use when the answer asserts facts about the world that the user may need to verify.",
            r#"Every factual sentence carries a bracketed number, like [1], pointing at the numbered
source list you end with.

Rules:
1. Cite the sentence that makes the claim, not the paragraph it sits in. A claim with no
   support in any source is the one thing this skill exists to prevent.
2. If no source supports a claim, either drop the claim or write it as
   "unverified — no source found". Never manufacture a citation to fill the gap.
3. A source is only a source if you read it. A title you recognise is not a source.
4. When sources disagree, cite both and state the disagreement rather than silently choosing.

End with the numbered list of what you actually read."#,
        ),
        (
            "tool-discipline",
            "Tool discipline",
            "Look before you leap: prefer one well-formed tool call over several guessed ones.",
            "Use whenever the agent holds tools — prefer it over improvising a call shape.",
            r#"A tool call is an operation on somebody's system, not a guess about one.

1. If a tool needs an argument you do not have, ask for it in plain text. Do not invent a
   value, and do not call the tool to see what error it returns.
2. One call at a time. A second call that depends on the first's result waits for it.
3. If a call fails, read the error before retrying. Retrying an identical call after an error is
   a loop, and the loop detector will end the run before you learn anything.
4. Quote the tool's own result when you rely on it. Never assert what a tool returned when it
   returned an error, and never smooth over a partial result into a confident summary."#,
        ),
    ];

    for (key, name, description, when_to_use, instructions) in seeds {
        let checksum = checksum_of(key, name, description, when_to_use, instructions, &[]);
        println!("{key}\t{checksum}");
    }
}
