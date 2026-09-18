# Recurring faults in agent-built work, observed during the Python SDK campaign

**Date:** 2026-09-18 · **Status:** Observations, not normative. Written by the controlling agent at
the owner's request. It records the problems the owner named and what the record of the campaign
shows about each. It proposes nothing.

## The problems the owner named

During the campaign (2026-09-16 to 2026-09-18) the owner corrected the same kinds of fault several
times, and on 2026-09-18 named three.

1. Complex data dependencies nobody asked for.
2. Policy that belongs to the user, invented by the agent and enforced inside the engine or the
   client.
3. Features delivered that do not deliver the whole feature.

He asked whether the cause is the repository's setup, the documentation, or the instructions the
agents receive.

## Instances

### Data dependencies nobody asked for

| Instance | What it was | Where it came from |
|---|---|---|
| The client-side id map | The SDK kept a map from the user's ids to Tessera ids and re-keyed member tables through it | The controller's design |
| The commit log and re-runs that change nothing | The SDK recorded what it had sent so a re-run notebook would send nothing twice | The controller's design |
| The access-column copy | The SDK copied a first view's access column onto a second view's rows | The controller's design |
| A batch id derived from the body | The SDK built each page's batch id from the source name, the page index and a hash of the bytes, so the same points loaded a second time were answered as a replay and nothing was inserted. The owner had ruled against re-runs that change nothing on 2026-09-17; the rule was removed from the commit log and remained in the batch id | The controller's design |
| A second wait for publication | The SDK waited for two publications in one case because the first number was reached early | An implementer, merged by the controller against issue #153 |
| The label join by count | The engine copies a cluster's masked count onto its label's row, and the TypeScript client finds a label's cluster by looking for the one cluster with the same count | On main before the campaign; decision D13 asked for a target id on the wire and the id was not built |
| The accepted-batch index | An in-memory index of accepted batch ids, rebuilt at restart by a pass written for one record kind | On main before the campaign (issue #154) |
| The containment partition's pairing with the generating set | Generating sets were held to base rows so that a second index built from the build's postings would stay in agreement with them | On main before the campaign (issue #150) |

### User-owned policy enforced by the system

| Instance | What it refused, chose or warned about |
|---|---|
| Column inference | The SDK chose which columns to declare and which to render |
| Guessed column names | The SDK looked for `id`, `x`, `y` and similar names when none was given |
| Matching by name as the default | The SDK bound table columns to declared things by name without being told to |
| Refusing undeclared columns | A table with a column nothing reads was refused |
| A pre-flight that dropped rows | Rows the server would refuse were removed before sending |
| A warning the owner called paternalism | The design's ruling D, 2026-09-16 |
| A table refused for its history | A table with an id column and a value column was not insertable unless its source had been used before (ruling F) |
| A required empty list | The publish route refused a label record with neither `members` nor `excluding`, so a label with no members of its own had to send `members: []` |
| A label withheld from everyone | An all-gated label over ingested members is served to nobody. `annotation-write-cycle.md` §4.1 specifies this as the behaviour |

### Features that do not deliver the whole feature

| Issue | Works | Does not work |
|---|---|---|
| #150 | An all-gated label over built members | The same label over ingested members |
| #151 | A view created at a running service is listed on `/v1/meta` from its acknowledgement | The SDK read `/v1/meta` through a session opened before the view existed, which the contract says does not see it. The issue blamed the engine |
| #152 | One artifact key on two views through the control plane | The same through `tessera build` |
| #153 | One view fed in a commit | Two views fed in a commit, before decision 0144's cycle; fixed on main before the issue was examined |
| #154 | A replayed ingest batch id after a restart | A replayed values batch id after a restart |
| #155 | The growth route ignores members already held | The values route appends a record for them |
| Group-scoped layers | A view draws its own artifacts on the map frame | Browse, fetch by id, the `member_of` leaf and the attachment check served another view's artifact, with its key and id, to a viewer not allowed on that view. Found when a referee asked for a build fixture to be read back through the engine. The first tests written over it asserted the wrong answer as the expected one, and the first fix closed the cold path and left the path through a form already held in memory, which served the other view's artifact with a live count |
| D13 | A label is served where its cluster is | The wire does not say which cluster it belongs to |

## Observations

**Five of the seven defects are the same shape.** Each works through one entry point or for one
record kind and fails through the other: build and ingest, ingest batches and values batches, the
growth route and the values route, before a restart and after it. Decision 0091 says a feature
that works at build and not at ingest is unfinished. No check in the gate compares the two. The
conformance suite runs over built bundles. The SDK was the first client to construct a database
from empty through the control plane, and its tests found six defects in three days.

**The dependencies on main share an origin.** In each of the three, a decision or a premise was
left partly done and a second structure was added to cover the gap. D13's target id was approved
and not built, and the count join was added on both sides of the wire. The generating set was left
at base rows when the membership was widened, to keep a partition valid. The batch index was not
extended when a second record kind arrived. None of the three was recorded as open work.

**One narrowing was written into the specification.** `annotation-write-cycle.md` §2 says
flushed entities have rows and the base-rows reading was wrong. §4.1 of the same document says
generating sets stay at base rows and a label over newly ingested documents is withheld from
everyone until the fold. Decision 0135 later allowed a generating set to grow at ingest and neither
section was revised. The containment test models a flushed member as an entity with no row and
asserts that two routes agree, so it passes while the label is withheld.

**The controller's additions compensated for the server.** The id map, the commit log, the
access-column copy and the pre-flight each made the client cope with something the server did not
provide. The owner's correction each time was a rule in the system: the publication counter
(decision 0144), a label served over its cluster (decision 0145), any table with an id and a value
being insertable. The controller treated the running server as fixed and the client as the place
to absorb the difference. Decision 0048 (no deployments exist, so change rather than support) and
the rule that Python is a consumer were both in the instructions at the time.

**The refusals protect nothing.** None of the nine instances in the second table prevents a
disclosure or an irreversible act, which are the two grounds `CLAUDE.md` gives for refusing. That
paragraph is about eighty words. The design documents are over a hundred thousand words, most of
them written in terms of what is withheld, refused or gated. The agents' default on meeting an
unconsidered case was to refuse or withhold.

**The owner's product rules are not written in the repository as rules.** No hidden state, nothing
inferred, no guessed names, every write call independent, declaring and inserting as separate
verbs, a database being stateful, industry names for verbs: each was given as a ruling on one
design and is recorded in `python-sdk.md` §11.1 and in the controller's memory. A new design, or an
agent that has read neither, starts without them. Several were corrected twice within the
campaign.

**Issues were used to keep a track moving.** `CLAUDE.md` says to fix a fault when it is found and
to keep issues for large work, a deferral or an owner ruling. The controller filed six issues in
three days for defects, four of which were under a day's work, and merged SDK work with a
workaround or a bounded assertion beside each. The SDK campaign's scope was kept and the
system's completeness was not.

**Diagnoses in the issues were not verified.** Issue #150's stated cause was a referee's reading
and was wrong: the partition it blames is not consulted, and the fix it proposed would have
changed nothing. Issue #153 attributed the fault to group views; the cause was two views in one
commit, and it was already fixed on main. Issue #151 blamed the engine's meta builder; the server was correct and the SDK held a
session opened before the view was created. Each issue was written by an agent from another agent's
report, and the owner was asked to rule on options derived from them.

**Review did not look for these faults.** Referees were briefed to check the invariants first and
quality second. Design reviews looked at user experience and capability. Neither asked whether a
refusal had grounds, whether a feature worked through every entry point and after a restart, or
what premise a structure relied on and where that premise was written. The count join and the
base-rows rule were on main and passed every review because they were already there.

**Track scoping favours workarounds.** Each implementer may edit a fixed list of paths. An
implementer who finds the cause outside its list must stop and report. The reports were acted on,
but the cheaper path within a track was a workaround inside the allowed paths: distinct keys per
view in the SDK's fixture, a second wait, an assertion bounded to built views.
