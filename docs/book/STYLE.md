# Writing Style Guide

For book, manual and website prose. [`CLAUDE.md`](../../CLAUDE.md) points here.


Analysed from *Chaos Engineering* (Manning, 2021) by Miko Pawlikowski. Key references used:
- [Chaos Engineering for People (Ch 13 excerpt)](https://www.linkedin.com/pulse/chaos-engineering-people-mikolaj-pawlikowski)
- [Breaking the top five myths around chaos engineering](https://www.cloudcomputing-news.net/news/breaking-the-top-five-myths-around-chaos-engineering/)
- [The First Cup of Chaos (Manning free article)](https://freecontent.manning.com/the-first-cup-of-chaos/)
- [Testing Apps in Real-World Conditions (Manning free article)](https://freecontent.manning.com/testing-apps-in-real-world-conditions/)
- [Chaos Engineering 2021 — Conf42 talk transcript](https://www.conf42.com/Chaos_Engineering_2021_Mikolaj_Pawlikowski_chaos_engineering_2021)
- [Break Things on Purpose podcast — Gremlin](https://www.gremlin.com/blog/podcast-break-things-on-purpose-mikolaj-pawlikowski-engineering-lead-at-bloomberg)

## Voice Summary

Miko writes like a knowledgeable colleague explaining something over coffee. Informal-authoritative. Direct, honest, confident without being arrogant. British English. Engineer's pragmatism: values clarity over elegance.

## DO — Miko's Natural Patterns

### 1. Vary sentence length dramatically
Long explanatory sentence, then short punchy one for emphasis.
- "Hundreds of different skills (hard and soft), a constantly shifting technological landscape, evolving requirements, personnel turnaround and a sheer scale of some organizations are all factors in how hard it can be to ensure no single points of failure. **She is a single point of failure.**"
- "I went looking for information. I wanted to understand *why*. [...] **So I wrote one.**"

### 2. Use contractions naturally
"it's", "don't", "they're", "won't", "didn't", "can't", "doesn't", "aren't", "you'll"
- "One problem with real life is that it's messy."
- "you don't need to remember the syntax of tc to implement it!"

### 3. Address the reader directly
Second person ("you", "your") and inclusive first person ("we", "let's").
- "Can you see where I'm going with this?"
- "You know your team, so do what you think works best."
- "Let's start with identifying single points of failure."

### 4. Ask rhetorical questions
Guide the reader's thinking by posing questions.
- "How do you think we could test them out?"
- "Would you do an inside job in production?"
- "Do you see any weak links?"

### 5. Prefer active voice
Name the agent. Say who did what.
- "Gruber discovered this in 1996" not "This was discovered by Gruber in 1996"
- "Studies show X" not "X has been shown"

### 6. Use "Now," and "After all," as transitions
- "Now, the team is made of individuals..."
- "After all, testing in production is an internet meme."
- "After all, to err is human."

### 7. Use dry, understated humour
Quick asides, not extended jokes. Self-deprecating. Move on immediately.
- "If only there was a methodology to uncover systemic problems in a distributed system... Oh wait!"
- "(spoiler: it's bad!)"

### 8. Define terms inline with parentheses on first use only
This is a genuine Miko pattern, but he does it once, not repeatedly.
- "(i.e. emergent properties of the system)"
- "(hard and soft)"

### 9. Lead with concrete examples, then extract principles
Describe the specific situation first, then define the concept. Never abstract-first.

### 10. Hedge honestly but briefly
Acknowledge uncertainty without throat-clearing.
- "it might be better to leave well alone"
- "The answer depends on many factors"
- NOT: "It is worth noting that" or "It should be emphasised that"

### 11. Use British English
"endeavour", "practise" (noun), "programme", "organisation", "colour", "recognised", "behaviour"

---

## DON'T — AI Patterns to Avoid

### 1. Em dash overuse
AI writes 15-40 em dashes (—) per chapter. Miko uses them sparingly. Replace most with commas, full stops, or parentheses. Keep only where they add genuine punch.

### 2. Formulaic sentence starters
NEVER start a sentence with: "Intriguingly,", "Reassuringly,", "Notably,", "Crucially,", "Importantly,", "Remarkably,", "Interestingly,"
These are the strongest AI tells. Let the content be interesting on its own.

### 3. Passive voice dominance
AI defaults to passive. Flip to active.
- BAD: "The leader is elected by Raft"
- GOOD: "Raft elects the leader"
- BAD: "It is now recognised as a single point of failure"
- GOOD: "We now recognise it as a single point of failure"

### 4. Excessive parenthetical definitions
AI defines every term inline, every time. Define once on first use per chapter, then trust the reader. Don't re-define "gossip", "lease" or "quorum" in every chapter.

### 5. Hedging throat-clearing
Delete entirely: "it is worth noting that", "it should be noted", "it is important to emphasise", "it is necessary to understand", "as previously mentioned"
Just say the thing.

### 6. Superlative stacking
AI piles on emphatic adjectives: "highly effective", "remarkably", "extraordinarily", "dramatic", "revolutionary", "transformative"
Let data speak for itself. "Failover in 2.1 s" beats "remarkably fast failover."

### 7. "Beyond X, Y" and "Whilst effective, Z"
These are formulaic AI transition patterns. Use natural transitions or just start the new idea.

### 8. Identical structural templates
AI makes every section follow the same pattern. Vary the structure: some sections can open with a question, some with a statistic, some with a historical anecdote, some with a direct statement.
