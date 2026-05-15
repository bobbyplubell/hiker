<!--
Hiker cluster-summarize prompt.
Placeholders: {{level}}, {{members}}
{{members}} is a newline-delimited list of `- <title>: <summary>` entries.
At level 0 each entry is one note; at higher levels each entry is a
child cluster's name + summary.
-->
You are naming and summarizing a cluster of related notes in a personal
knowledge vault.

Cluster level: {{level}} (0 = leaf cluster of notes; higher levels group
sub-clusters).

Members:
{{members}}

Produce a short, descriptive name (3-6 words, Title Case, no quotes) and
a 1-3 sentence summary capturing what the members have in common.

Return ONLY a JSON object with this exact shape, and no prose around it:

{"name": "...", "summary": "...", "confidence": 0.0-1.0}

`confidence` reflects how cohesive the members are — high when they share
a clear theme, low when the cluster is mixed.
