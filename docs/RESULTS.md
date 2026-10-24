# CodeLore against the markdown it replaces - the first measurement

Measured 2026-10-24 on CodeLore's first production user - a private
repository of tools that several agent sessions work in at once - by a bench
script kept in that repository. The question, its owner's: which is faster
and more effective for an agent - CodeLore, or the markdown structure the
project kept until now.

## What was measured

**The knowledge**: one section of the project's troubleshooting page - 42
table rows of faults met while several agent sessions shared one git working
tree - written by hand as 37 CodeLore records (rows that were one fault
merged).

**The questions**: 82, two for each of the section's 41 faults -
- *exact*: the text the fault prints, as an agent would paste it;
- *paraphrase*: the symptom in other words, held by a check to share no
  three-word run and no quoted name with the row it is about.

Each question carries **keys**: strings from the row's fix. A result answers
when it holds one; every system is judged the same way.

**The systems**, each as an agent meets it:
- **grep** - `git grep` over the project's `docs/` (20.7 MB of markdown), fixed
  string for an exact question, the two rarest words for a paraphrase or a
  miss; a result is the matched line with two lines either side.
- **code index** - the project's semantic code index (hybrid dense + BM25 over
  100-line chunks), its search reproduced against its own vector store and
  checked against the real tool: the same files, lines and scores.
- **lore** - `lore find -n 10`, at full size: the 37 records among 1 490 other
  rows of the same page converted mechanically, so the records are searched
  among as many as the whole migration will make. `lore-text` is the same
  without the embedding model.

## The numbers

| system | question | answer first | answer in top 3 | in top 10 | MRR | read to the answer, median | p90 | calls |
|---|---|---|---|---|---|---|---|---|
| grep | exact | 61% | 76% | 90% | 0.71 | 4 972 B | 10 382 B | 1.39 |
| grep | paraphrase | 7% | 7% | 10% | 0.08 | 9 107 B | 49 998 B | 1.00 |
| code index | exact | 39% | 61% | 78% | 0.52 | 4 116 B | 17 341 B | 1.00 |
| code index | paraphrase | 24% | 29% | 51% | 0.30 | 13 024 B | 20 073 B | 1.00 |
| **lore** | exact | **98%** | **100%** | **100%** | **0.98** | **1 011 B** | **1 662 B** | 1.00 |
| **lore** | paraphrase | **71%** | **83%** | **95%** | **0.78** | **1 123 B** | **5 516 B** | 1.00 |
| lore-text | exact | 95% | 100% | 100% | 0.97 | 1 003 B | 1 667 B | 1.00 |
| lore-text | paraphrase | 56% | 66% | 85% | 0.65 | 1 654 B | 11 498 B | 1.00 |

"Read to the answer" is the bytes of results up to and including the first
that answers - everything shown, on a miss. A quarter of it is roughly tokens:
lore's median is about 260 tokens, grep's 1 250-2 300, the code index's
1 000-3 300.

Wall time per call: grep 0.07 s, code index 0.05 s, lore 0.75 s (0.6 s text
only).

## Why lore is faster to the answer

"Faster" for an agent is not milliseconds per call - it is how many calls and
how much reading it takes before the fix is on the screen. On that measure
lore wins, for five reasons, each visible in the table.

1. **The unit is the answer.** A record is a symptom, its cause, its fix and
   the check that sees it - about 1 KB. grep returns lines of a table whose
   rows run to 3.8 KB, with context around them; the code index returns 100-line
   chunks cut wherever the line count falls, often through the middle of a
   row. So lore's first answer costs 4-13 times less reading, and its p90 is
   smaller than the others' median.
2. **The answer comes first.** 98% of exact questions and 71% of paraphrases
   get the right record first; grep manages 61% and 7%, the code index 39% and
   24%. Every result above the answer is reading and, often, another call.
3. **A description finds its fault.** grep is literal: a symptom told in other
   words finds the right row one time in ten. lore ranks by meaning (a local
   embedding of each record's sentence), by stemmed words and by whole code
   terms together; the code index embeds too, but its chunks mix several faults,
   so the meaning of each is diluted.
4. **grep's strength is kept.** An error pasted verbatim hits a record's
   `signals` - the exact strings the fault prints - and a copied phrase hits a
   record that says it word for word; both outrank everything else. That is
   why lore beats grep on exact questions too, instead of trading them for
   paraphrases.
5. **One call.** grep needs a second call on 39% of exact questions (the
   pasted text was not in the docs as typed); lore answers in one.

Per call, lore is slower - 0.75 s against 0.05-0.08 s: a Python process starts
and the question is embedded. Against an agent's turn, which is seconds of
model time per thousand tokens read, the 0.7 s is paid back by the first
kilobyte not read.

## What the numbers do not say yet

- **The records were written knowing the questions.** The same person wrote
  the 82 questions and then the 37 records; a writer who knows what will be
  asked may phrase records to match. The rest of the migration is held to the
  same keys but not to these questions, and the agents' test below removes
  the writer from the loop.
- **One change came from this run.** A copied phrase ranked its own record
  below ten others at full size; lore 0.1.2 added the phrase match because of
  it. It is grep's own rule and not tuned to a question, but it was made after
  seeing the miss.
- **The other 1 490 rows were converted mechanically**, not written as
  records; a hand-written corpus of the same size may hold more look-alikes.
- **41 faults, one section, one project.**
- **The agents' test is not run**: the same tasks given to fresh agents on the
  old tree and on the lore tree, measuring correct answers, tokens, calls and
  time. It is the one that decides.

## What the pilot found in lore itself

Three faults, each fixed with a test (0.1.1, 0.1.2):
- stdin was read in the console's code page, so on Windows «» and … reached
  the records as two or three characters each;
- the duplicate search refused on embedding similarity, which on this corpus
  does not separate duplicates (0.69-0.85) from different faults (0.87-0.88);
  it refuses now only on strong evidence and names neighbours otherwise;
- a copied phrase lost to a bag of its own common words.
