"""Toponymy — a layered clustering over the arXiv rung, named by a language model.

The rung's **optional second stage**. It reads the directory `prepare.py` wrote, adds two layers to
it, and splices its declaration into the copy of `corpus.toml` beside the data. It is a stage of
its own rather than a flag because it needs a chat endpoint and because it costs the run: over the
whole corpus the first stage is twenty minutes and this one is an hour and a half, almost all of it
waiting on the model. Running it twice over one `prepare.py` output is the ordinary way to try a
different floor or a different model.

[Toponymy](https://github.com/TutteInstitute/toponymy) is the reference pipeline for naming the
clusters of a data map, and this runs it as it is meant to be run rather than imitating its output.
Three things it does are worth knowing before reading the code.

**It clusters in layers, and the layers are the hierarchy.** HDBSCAN over the projection at a
ladder of minimum cluster sizes — the finest rung at `--min-cluster`, each coarser rung at the 85th
percentile of the previous rung's sizes — and every rung is kept. A cluster on one rung is claimed
by the cluster on a coarser rung holding most of its points, so the edges run *between* levels.
Unlike `taxonomy/arxiv` it is not covering — a paper that is noise on a rung belongs to nothing
there — and unlike the condensed tree a point can be noise on a fine rung and a member on a coarser
one.

**It names from three things, and coarser rungs are named from finer ones.** For each cluster the
prompt carries a handful of *exemplar* papers (the titles nearest the centroid in embedding space),
a list of *keyphrases* (n-grams from the corpus's own titles, ranked by how much more they occur in
the cluster than outside it, diversified by embedding), and — above the finest rung — the *names*
of the clusters beneath it as subtopics. Then it asks for a name and re-asks where names collide.

**The generating set is the prompt's sample, and that is a decision rather than a convenience.**
The label a viewer is served was written from the exemplars the model saw; the keyphrases were
counted over the whole cluster, but a count is a statistic and not a document, and no title is
disclosed by a phrase that occurs in thousands. So a name's generating set is its exemplars — the
papers whose titles the model was shown — and a viewer who can read every one of them may read the
name. `docs/evidence/prior-art/prior-art-synthesis.md` asked for this to be decided and written
down; it is decided here.

**The model is local, and reasoning is off.** Any OpenAI-compatible chat endpoint; the default is
the one this machine hosts. A chat template that thinks before it answers spends most of its tokens
on the thinking, and the JSON grammar the wrapper asks for then constrains the wrong text, so the
wrapper passes `enable_thinking = false`.

⊘ **From WSL2 the Windows loopback is not reachable in the default NAT networking mode**, and both
Unsloth Studio (`:8888`, which also wants a bearer token) and a llama-server behind it bind to
`127.0.0.1` on the Windows side. Either put WSL into mirrored networking, forward a port on the
Windows side, or point `--url` at wherever the model actually answers. This stage pings the
endpoint before it spends anything on keyphrases, so a wrong URL fails in the first second rather
than the last.

`--llm mock` runs the whole pass with a stand-in that names a cluster after its first keyphrase. It
exists so the plumbing — the layers, the edges, the generating sets, the declaration — can be
checked in seconds without a model on the other end. Its labels are not labels, and the manifest
says so.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import re
import sys
import time
from pathlib import Path

import numpy as np
import pyarrow.parquet as pq

from ..common.paths import ladder
from ..common.timing import Steps
from . import sources
from .prepare import RUNG
from .writer import ArtifactSet

TOPONYMY_LAYER, TOPONYMY_LABELS = "clusters/toponymy", "topics/toponymy"

#: The text encoder the keyphrase step uses. It has to be the model the paper embeddings came from,
#: because keyphrases are chosen by where they land *among those vectors*.
EMBEDDER = "BAAI/bge-large-en-v1.5"

#: The per-cluster prompt budget. A local model's slot is small — 4,096 tokens with the server's
#: context split four ways — and a disambiguation prompt lists several clusters with everything
#: each carries, so Toponymy's defaults (8, 16, 16) overran it.
N_EXEMPLARS, N_KEYPHRASES, N_SUBTOPICS = 6, 12, 12


def min_cluster_size(n: int) -> int:
    """Toponymy's finest rung. Every cluster on every rung is one model call, so this is the knob
    that prices the run: lower means more, smaller leaves and calls in proportion."""
    return max(50, n // 2000)


# ------------------------------------------------------------------------------- the two namers


def _wrappers():
    """Imported inside a function so that `--help` works without the optional dependencies."""
    import openai
    from toponymy.llm_wrappers import AsyncLLMWrapper, LLMWrapper

    class LocalOpenAINamer(AsyncLLMWrapper):
        """Toponymy's async wrapper shape over any OpenAI-compatible chat endpoint.

        Two departures from the library's own `AsyncOpenAINamer`, both about talking to a local
        model: the base URL actually reaches the client, and `enable_thinking` is switched off
        through the chat template so the reply is the JSON the prompt asks for rather than a
        reasoning trace with the JSON somewhere after it.
        """

        FAIL_FAST_EXCEPTIONS = (
            openai.AuthenticationError,
            openai.PermissionDeniedError,
            openai.NotFoundError,
            openai.UnprocessableEntityError,
            openai.BadRequestError,
        )
        _supports_debug_callback = True
        supports_system_prompts = True

        #: A prompt the slot cannot hold is cut to this many characters of user text and sent once
        #: more, rather than failing the layer. Counted, and reported in the manifest: a name
        #: written from a truncated prompt is still a name, but the run should say how many.
        TRUNCATE_TO = 9_000

        def __init__(self, base_url, api_key, model, max_concurrent_requests):
            self.base_url, self.api_key = base_url, api_key or "none"
            self.model = model
            self.callback = None
            self.extra_prompting = ""
            self.calls = 0
            self.truncated = 0
            self.max_concurrent_requests = max_concurrent_requests
            self._per_loop = {}

        def _bound(self):
            """The client and the semaphore for *this* event loop. Toponymy runs each layer's
            batch on a loop of its own, and both objects bind to the loop they were first used on
            — one made in the constructor served the first layer and failed every call of the
            second."""
            loop = asyncio.get_running_loop()
            if loop not in self._per_loop:
                self._per_loop[loop] = (
                    openai.AsyncOpenAI(base_url=self.base_url, api_key=self.api_key, timeout=600),
                    asyncio.Semaphore(self.max_concurrent_requests),
                )
            return self._per_loop[loop]

        async def _chat(self, messages, temperature, max_tokens):
            self.calls += 1
            client, semaphore = self._bound()
            async with semaphore:
                try:
                    return await self._create(client, messages, temperature, max_tokens)
                except openai.BadRequestError as e:
                    if "context_length" not in str(e):
                        raise
                    self.truncated += 1
                    cut = [
                        dict(m, content=m["content"][: self.TRUNCATE_TO])
                        if m["role"] == "user"
                        else m
                        for m in messages
                    ]
                    return await self._create(client, cut, temperature, max_tokens)

        async def _create(self, client, messages, temperature, max_tokens):
            response = await client.chat.completions.create(
                model=self.model,
                messages=messages,
                temperature=temperature,
                max_tokens=max_tokens,
                response_format={"type": "json_object"},
                extra_body={"chat_template_kwargs": {"enable_thinking": False}},
            )
            return response.choices[0].message.content

        async def _call_single_llm(self, prompt, temperature, max_tokens):
            return await self._chat([{"role": "user", "content": prompt}], temperature, max_tokens)

        async def _call_single_llm_with_system(self, system_prompt, user_prompt, temperature,
                                               max_tokens):
            return await self._chat(
                [
                    {"role": "system", "content": system_prompt},
                    {"role": "user", "content": user_prompt},
                ],
                temperature,
                max_tokens,
            )

    class MockNamer(LLMWrapper):
        """A stand-in for the plumbing check: a group is named after its first keyphrase, and a
        disambiguation request numbers the names it was given. Nothing it writes is a label."""

        model = "mock"

        def __init__(self):
            self.callback = None
            self.calls = 0
            self.truncated = 0

        def _answer(self, text):
            self.calls += 1
            if "new_topic_name_mapping" in text:
                olds = re.findall(r'^"(\d+\. .+?)":$', text, re.M)
                return json.dumps(
                    {
                        "new_topic_name_mapping": {
                            o: f"{o.split('. ', 1)[1]} ({o.split('.')[0]})" for o in olds
                        },
                        "topic_specificities": [0.5] * len(olds),
                    }
                )
            found = re.search(r"Keywords for this group include: (.+)", text)
            name = found.group(1).split(",")[0].strip() if found else "unnamed"
            return json.dumps({"topic_name": name, "topic_specificity": 0.5})

        def _call_llm(self, prompt, temperature, max_tokens):
            return self._answer(prompt)

        def _call_llm_with_system_prompt(self, system_prompt, user_prompt, temperature, max_tokens):
            return self._answer(system_prompt + "\n" + user_prompt)

    return LocalOpenAINamer, MockNamer


def ping(url: str, key: str, model: str) -> str:
    """Fail here, with the endpoint named, rather than three minutes into keyphrase extraction."""
    import openai

    reply = openai.OpenAI(base_url=url, api_key=key or "none", timeout=120).chat.completions.create(
        model=model,
        max_tokens=32,
        temperature=0.0,
        messages=[
            {
                "role": "user",
                "content": 'Reply with exactly this JSON and nothing else: {"status": "ok"}',
            }
        ],
        response_format={"type": "json_object"},
        extra_body={"chat_template_kwargs": {"enable_thinking": False}},
    )
    return reply.choices[0].message.content


class TimedEncoder:
    """The encoder, with a meter on it: Toponymy encodes keyphrases, names and subtopics at several
    points, and the manifest should say what that cost."""

    def __init__(self, model):
        self.model, self.seconds, self.texts = model, 0.0, 0

    def encode(self, texts, *args, **kwargs):
        at = time.time()
        vectors = self.model.encode(texts, *args, **kwargs)
        self.seconds += time.time() - at
        self.texts += len(texts)
        return vectors


# -------------------------------------------------------------------------- splicing the config


def splice_declaration(out: Path, levels: str) -> None:
    """Inject this stage's sources and layer into the copy of the declaration beside the data.

    The git `corpus.toml` is never touched: `prepare.py` copies it, and this edits the copy. A
    missing marker is a refusal — a splice that silently did nothing would leave a build reading a
    declaration with no Toponymy layer while this stage's files sat beside it.
    """
    fragment = (Path(__file__).parent / "toponymy.toml").read_text()
    marker = "\n# ---- split ----\n"
    assert marker in fragment, "toponymy.toml has lost its split marker"
    source_lines, layer_block = fragment.split(marker, 1)
    source_lines = "\n".join(
        line for line in source_lines.splitlines() if line and not line.startswith("#")
    )

    assert "# <levels>" in layer_block, "toponymy.toml has lost its levels marker"
    layer_block = layer_block.replace("# <levels>", levels)

    declaration = (out / "corpus.toml").read_text()
    assert "# <toponymy-sources>" in declaration, (
        f"{out / 'corpus.toml'} has lost its sources marker — rerun prepare.py"
    )
    declaration = declaration.replace("# <toponymy-sources>", source_lines)
    (out / "corpus.toml").write_text(declaration.rstrip() + "\n" + layer_block)


# ------------------------------------------------------------------------------------- the run


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--out", type=Path, default=None, help=f"default $TESSERA_LADDER/{RUNG}")
    ap.add_argument("--llm", choices=("live", "mock"), default="live",
                    help="'mock' names a cluster after its first keyphrase, for checking plumbing")
    ap.add_argument("--url", default="http://127.0.0.1:8888/v1")
    ap.add_argument("--key", default="")
    ap.add_argument("--model", default="unsloth/Qwen3.6-35B-A3B-MTP-GGUF")
    ap.add_argument("--concurrency", type=int, default=4,
                    help="requests in flight; 4 is the llama-server's --parallel")
    ap.add_argument("--min-cluster", type=int, default=None,
                    help="the finest rung's floor; default max(50, n / 2000)")
    ap.add_argument("--keyphrases", type=int, default=20_000,
                    help="candidate keyphrase vocabulary")
    args = ap.parse_args()

    out = args.out or ladder(RUNG)
    points_file = out / "points.parquet"
    assert points_file.exists(), f"no points file at {points_file} — run prepare.py first"
    steps = Steps()
    print(f"corpus  {out}\nllm     {args.llm}: {args.model} at {args.url}")

    # ------------------------------------------------------------------ what the first stage wrote
    with steps.step("load points"):
        points = pq.read_table(points_file, columns=["entity_id", "x", "y", "arxiv_id", "title"])
        n = points.num_rows
        xy = np.column_stack(
            [points.column("x").to_numpy(), points.column("y").to_numpy()]
        ).astype(np.float32)
        titles = points.column("title").to_pylist()

    with steps.step("load embeddings"):
        corpus, _ = sources.load_metadata()
        take = sources.rows_for_ids(corpus, points.column("arxiv_id").to_pylist())
        X = sources.load_embeddings(corpus, take)
    print(f"{n:,} papers, {X.shape[1]} dimensions")

    # ---------------------------------------------------------------------- the layered clustering
    from toponymy import Toponymy
    from toponymy.clustering import ToponymyClusterer
    from toponymy.keyphrases import KeyphraseBuilder
    from toponymy.templates import PROMPT_TEMPLATES

    floor = args.min_cluster or min_cluster_size(n)
    with steps.step("layered clustering"):
        clusterer = ToponymyClusterer(
            base_min_cluster_size=floor,
            min_samples=10,
            min_clusters=6,
            next_cluster_size_quantile=0.85,
            verbose=True,
        )
        # Fitted here rather than inside `Toponymy.fit`, so the rung sizes are known — and reported
        # — before a single model call is spent on them.
        clusterer.fit(xy, X, prompt_template=PROMPT_TEMPLATES, n_exemplars=N_EXEMPLARS,
                      n_keyphrases=N_KEYPHRASES, n_subtopics=N_SUBTOPICS)

    layers = clusterer.cluster_layers_  # finest first, which is Toponymy's layer 0
    rungs = len(layers)

    def level(layer_index: int) -> int:
        """**Levels count down from the coarsest rung**, so a parent's level is always the smaller
        number — the convention `taxonomy/arxiv` uses, and the direction the build resolves a
        tiered edge in."""
        return rungs - 1 - layer_index

    sizes = [np.bincount(layer.cluster_labels[layer.cluster_labels >= 0]) for layer in layers]
    noise = [float((layer.cluster_labels < 0).mean()) for layer in layers]
    print(f"finest rung at min_cluster_size {floor}: {rungs} rungs, "
          f"{sum(len(s) for s in sizes)} clusters to name in all")
    for i, (rung_sizes, rung_noise) in enumerate(zip(sizes, noise)):
        print(f"  level {level(i)}  {len(rung_sizes):>5} clusters, "
              f"sizes {rung_sizes.min():,}..{rung_sizes.max():,}, "
              f"{rung_noise:.1%} of papers in none")

    # --------------------------------------------------------------------------------- the naming
    LocalOpenAINamer, MockNamer = _wrappers()
    if args.llm == "mock":
        namer = MockNamer()
    else:
        namer = LocalOpenAINamer(args.url, args.key, args.model, args.concurrency)
        print(f"{args.model} at {args.url} says {ping(args.url, args.key, args.model).strip()[:80]}")

    with steps.step("text encoder"):
        from sentence_transformers import SentenceTransformer

        # CPU is deliberate — the GPU is where the language model lives.
        embedder = TimedEncoder(SentenceTransformer(EMBEDDER, device="cpu"))

    with steps.step("keyphrases and names"):
        topic_model = Toponymy(
            llm_wrapper=namer,
            text_embedding_model=embedder,
            clusterer=clusterer,
            keyphrase_builder=KeyphraseBuilder(max_features=args.keyphrases, verbose=True),
            object_description="arXiv papers, given by their titles",
            corpus_description="a collection of arXiv preprints across every field arXiv covers",
            verbose=True,
        )
        topic_model.fit(titles, X, xy)

    # A name per cluster, the exemplars it was written from, and the keyphrases the model was
    # offered. Exemplar indices are rows of this corpus — the generating set.
    names = [list(layer.topic_names) for layer in layers]
    exemplars = [[list(map(int, idx)) for idx in layer.exemplar_indices] for layer in layers]
    keyphrases = [[list(k) for k in layer.keyphrases] for layer in layers]

    print(f"{namer.calls} model calls, {namer.truncated} of them on a truncated prompt; "
          f"{embedder.texts:,} texts encoded in {embedder.seconds:.0f}s")
    for i in range(rungs):
        empty = sum(1 for name in names[i] if not name)
        print(f"level {level(i)}: {len(names[i])} names"
              + (f", {empty} EMPTY — the model failed them" if empty else ""))
        for c in range(min(4, len(names[i]))):
            print(f"   {sizes[i][c]:>7,}  {names[i][c]!r:50}  ← {', '.join(keyphrases[i][c][:3])}")

    # -------------------------------------------------------------------------- the build inputs
    artifacts = ArtifactSet()

    def key(layer_index, c):
        return f"tp{level(layer_index)}-{c:06d}"

    # A cluster's parent is the cluster on a coarser rung that Toponymy's own tree claimed it for;
    # the synthetic root the library adds above its top rung is not a cluster and is not written.
    parent_of = {}
    for (parent_layer, parent_c), kids in clusterer.cluster_tree_.items():
        if parent_layer >= rungs:
            continue
        for kid in kids:
            parent_of[tuple(kid)] = (parent_layer, parent_c)

    with steps.step("write artifacts"):
        for i in range(rungs):
            for c in range(len(names[i])):
                parent = parent_of.get((i, c))
                artifacts.artifact(TOPONYMY_LAYER, key(i, c), level=level(i),
                                   parent=key(*parent) if parent else None)
                # A label's two contents are the model's name and, beneath it, the three
                # keyphrases the model was shown first.
                fallback = ", ".join(keyphrases[i][c][:3]) or "a cluster of papers"
                artifacts.artifact(TOPONYMY_LABELS, f"tpl{level(i)}-{c:06d}",
                                   contents=[[names[i][c] or fallback], [fallback]],
                                   attached=(TOPONYMY_LAYER, level(i), key(i, c)))

    with steps.step("write members"):
        for i in range(rungs):
            on_rung = layers[i].cluster_labels
            for c in range(len(names[i])):
                artifacts.members(TOPONYMY_LAYER, key(i, c),
                                  np.flatnonzero(on_rung == c), level=level(i))
        # A name's generating set is the exemplars the model was shown, and its fallback's is the
        # first half of them — satisfiable by a narrower principal, which is what the ranking is
        # for. The label layer is flat, so every one of these rows sits at level 0.
        for i in range(rungs):
            for c in range(len(names[i])):
                shown = exemplars[i][c]
                label_key = f"tpl{level(i)}-{c:06d}"
                artifacts.members(TOPONYMY_LABELS, label_key, shown)
                artifacts.members(TOPONYMY_LABELS, label_key, shown, rank=0)
                artifacts.members(TOPONYMY_LABELS, label_key,
                                  shown[: max(1, len(shown) // 2)], rank=1)

    # Every edge lands on a strictly coarser level, which is what a tiered layer promises the
    # build; and every name has one — an empty name is the model having failed three times, which
    # the fallback content covers but the manifest should be read for.
    for (child_layer, _), (parent_layer, _) in parent_of.items():
        assert level(parent_layer) < level(child_layer), (
            f"a Toponymy edge runs against the resolution at level {level(child_layer)}"
        )
    artifacts.check(n)
    artifact_rows, member_rows = artifacts.write(out)
    print(f"{artifact_rows:,} artifacts, {member_rows:,} member rows over {rungs} rungs")

    # ------------------------------------------------------------------ the declaration and manifest
    # A rung's title says what it is — how many clusters, and the size floor they were found at, is
    # what an operator picking a level wants to know.
    levels_block = "\n".join(
        f'  [[layer.levels]]\n  level = {level(i)}\n'
        f'  title = "{len(sizes[i])} topics, {int(sizes[i].min()):,}+ papers each"\n'
        for i in reversed(range(rungs))
    )
    splice_declaration(out, levels_block)

    manifest_file = out / "manifest.json"
    manifest = json.loads(manifest_file.read_text())
    manifest["toponymy"] = {
        "min_cluster_size": int(floor),
        "rungs": rungs,
        "clusters_per_level": {level(i): len(s) for i, s in enumerate(sizes)},
        "noise_share_per_level": {level(i): round(v, 4) for i, v in enumerate(noise)},
        "keyphrase_vocabulary": len(topic_model.keyphrase_list_),
        "exemplars_per_name": N_EXEMPLARS,
        "model_calls": namer.calls,
        "truncated_prompts": namer.truncated,
        "encode_texts": embedder.texts,
        "encode_seconds": round(embedder.seconds, 1),
        "llm": {
            "mode": args.llm,
            "model": args.model if args.llm == "live" else None,
            "url": args.url if args.llm == "live" else None,
            "labels_are_real": args.llm == "live",
        },
        "embedder": EMBEDDER,
        "artifact_rows": artifact_rows,
        "member_rows": member_rows,
        "seconds": dict(steps),
        "total_seconds": steps.total(),
    }
    manifest_file.write_text(json.dumps(manifest, indent=2) + "\n")

    print(f"\nspliced the layer into {out / 'corpus.toml'}")
    print(f"\nnext:\n  cd {out}\n  tessera check\n  tessera build")


if __name__ == "__main__":
    sys.exit(main())
