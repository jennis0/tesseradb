"""Render the candidates over the real clusters, because this question is visual.

Four figures, written to `figures/`:

- `layer-<family>.png` — every artifact of the layer, that family's shape drawn over the whole
  corpus. This is the picture the owner's *"really ugly"* was about.
- `families-<key>.png` — one artifact, one panel per family, members underneath.
- `multimodal-<key>.png` — the artifacts whose members are in several separated components, which
  is where a single ring has to lie.
- `alpha-<key>.png` — one artifact at several α, the parameter the engine fixes at 3.

    python3 figures.py
"""

import os
import pickle
import sys

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
import numpy as np  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from load import Layer, corpus as corpus_positions  # noqa: E402
from shapes import (  # noqa: E402
    _alpha_of,
    alpha_complex,
    alpha_dig,
    buffered_union,
    chaikin,
    chi_shape,
    convex,
    covariance_ellipse,
    density_level_set,
    knn_hull,
    simplify_ring,
)

HERE = os.path.dirname(os.path.abspath(__file__))
FIGS = os.path.join(HERE, "figures")
INK = "#111318"
POINT = "#8899aa"
LINE = "#e8543f"


def draw_rings(ax, rings, colour=LINE, lw=1.2, fill=0.0):
    for r in rings:
        if len(r) < 2:
            ax.plot(r[:, 0], r[:, 1], ".", color=colour, ms=3)
            continue
        loop = np.vstack([r, r[:1]])
        if fill:
            ax.fill(loop[:, 0], loop[:, 1], color=colour, alpha=fill, lw=0)
        ax.plot(loop[:, 0], loop[:, 1], "-", color=colour, lw=lw)


def frame(ax, title):
    ax.set_title(title, color="#dde3ea", fontsize=9, pad=6)
    ax.set_xticks([])
    ax.set_yticks([])
    ax.set_facecolor(INK)
    for s in ax.spines.values():
        s.set_color("#2a2f38")
    ax.set_aspect("equal")


def layer_figure(layer, rings_by_key, family, corpus, name):
    fig, ax = plt.subplots(figsize=(11, 11), facecolor=INK)
    ax.scatter(corpus[:, 0], corpus[:, 1], s=0.12, c="#6f8296", lw=0, alpha=0.5, rasterized=True)
    for key, fams in rings_by_key.items():
        if key == "hdb-2422486":  # the root holds the whole corpus; its ring is the map's frame
            continue
        draw_rings(ax, fams[family], colour=LINE, lw=0.7, fill=0.05)
    frame(ax, name)
    fig.tight_layout()
    out = os.path.join(FIGS, f"layer-{family}.png")
    fig.savefig(out, dpi=130, facecolor=INK)
    plt.close(fig)
    print("wrote", out)


def families_figure(p, key):
    """One artifact, every family. The sampled families are labelled: a shape over a sample is a
    different object from a shape over the membership (decision 0099), and is drawn only to show
    what the family looks like."""
    alpha = _alpha_of(p)
    panels = [
        ("convex wrap", convex(p), True),
        ("alpha shape (as built)", alpha_dig(p), True),
        ("alpha-complex", alpha_complex(p, alpha=alpha)[0], True),
        ("chi-shape", chi_shape(p, alpha=alpha), True),
        ("k-NN hull (Moreira-Santos)*", knn_hull(p), True),
        ("buffered union*", buffered_union(p), False),
        ("covariance ellipse (2 sigma)", covariance_ellipse(p), False),
        ("density level set", density_level_set(p), False),
        ("chi-shape, Douglas-Peucker", simplify_ring(chi_shape(p, alpha=alpha), alpha / 2), True),
        ("chi-shape, Chaikin smoothed", chaikin(chi_shape(p, alpha=alpha)), False),
    ]
    cols = 5
    rows = (len(panels) + cols - 1) // cols
    fig, axes = plt.subplots(rows, cols, figsize=(3.1 * cols, 3.3 * rows), facecolor=INK)
    for ax, (title, rings, member_vertices) in zip(np.ravel(axes), panels):
        ax.scatter(p[:, 0], p[:, 1], s=0.4, c=POINT, lw=0, rasterized=True)
        draw_rings(ax, rings, fill=0.10)
        mark = "" if member_vertices else "  [invented vertices]"
        frame(ax, f"{title}{mark}\n{sum(len(r) for r in rings)} vertices, {len(rings)} ring(s)")
    for ax in np.ravel(axes)[len(panels) :]:
        ax.axis("off")
        ax.set_facecolor(INK)
    fig.suptitle(
        f"{key} — {len(p):,} members   (* sampled: 800 for the k-NN walk, 1,500 for the buffer)",
        color="#dde3ea",
        fontsize=11,
    )
    fig.tight_layout()
    out = os.path.join(FIGS, f"families-{key}.png")
    fig.savefig(out, dpi=130, facecolor=INK)
    plt.close(fig)
    print("wrote", out)


def alpha_figure(p, key, factors=(1, 2, 3, 5, 8)):
    fig, axes = plt.subplots(2, len(factors), figsize=(3.1 * len(factors), 6.6), facecolor=INK)
    med = _alpha_of(p, factor=1.0)
    for col, f in enumerate(factors):
        for row, (label, rings) in enumerate(
            [
                (f"alpha shape, alpha = {f}x", alpha_dig(p, factor=f)),
                (f"chi-shape, alpha = {f}x", chi_shape(p, alpha=f * med)),
            ]
        ):
            ax = axes[row][col]
            ax.scatter(p[:, 0], p[:, 1], s=0.4, c=POINT, lw=0, rasterized=True)
            draw_rings(ax, rings, fill=0.10)
            frame(ax, f"{label}\n{sum(len(r) for r in rings)} vertices")
    fig.suptitle(
        f"{key} — {len(p):,} members. The engine fixes alpha at 3x the median wrap edge.",
        color="#dde3ea",
        fontsize=11,
    )
    fig.tight_layout()
    out = os.path.join(FIGS, f"alpha-{key}.png")
    fig.savefig(out, dpi=130, facecolor=INK)
    plt.close(fig)
    print("wrote", out)


def multimodal_figure(layer, keys):
    fig, axes = plt.subplots(len(keys), 4, figsize=(12.5, 3.3 * len(keys)), facecolor=INK)
    axes = np.atleast_2d(axes)
    for row, key in enumerate(keys):
        p = np.unique(layer.members(layer.keys.index(key)).astype(np.float64), axis=0)
        alpha = _alpha_of(p)
        for col, (title, rings) in enumerate(
            [
                ("convex wrap", convex(p)),
                ("alpha shape (as built)", alpha_dig(p)),
                ("chi-shape", chi_shape(p, alpha=alpha)),
                ("alpha-complex (many rings)", alpha_complex(p, alpha=alpha)[0]),
            ]
        ):
            ax = axes[row][col]
            ax.scatter(p[:, 0], p[:, 1], s=0.6, c=POINT, lw=0, rasterized=True)
            draw_rings(ax, rings, fill=0.10)
            frame(ax, f"{key}  {title}\n{sum(len(r) for r in rings)} vertices, {len(rings)} ring(s)")
    fig.suptitle("Members in several separated components, and what one ring does with them", color="#dde3ea")
    fig.tight_layout()
    out = os.path.join(FIGS, "multimodal.png")
    fig.savefig(out, dpi=130, facecolor=INK)
    plt.close(fig)
    print("wrote", out)


def side_by_side(layer, keys):
    """The comparison the recommendation rests on: what `main` serves, against the χ-shape."""
    fig, axes = plt.subplots(3, len(keys), figsize=(2.7 * len(keys), 8.6), facecolor=INK)
    for col, key in enumerate(keys):
        p = np.unique(layer.members(layer.keys.index(key)).astype(np.float64), axis=0)
        alpha = _alpha_of(p)
        for row, (title, rings) in enumerate(
            [
                ("convex wrap", convex(p)),
                ("alpha shape (main)", alpha_dig(p)),
                ("chi-shape", chi_shape(p, alpha=alpha)),
            ]
        ):
            ax = axes[row][col]
            ax.scatter(p[:, 0], p[:, 1], s=0.3, c=POINT, lw=0, rasterized=True)
            draw_rings(ax, rings, fill=0.10)
            frame(ax, f"{key}\n{title} — {sum(len(r) for r in rings)}v")
    fig.suptitle("What main serves, and the same alpha peeled instead of dug", color="#dde3ea")
    fig.tight_layout()
    out = os.path.join(FIGS, "side-by-side.png")
    fig.savefig(out, dpi=130, facecolor=INK)
    plt.close(fig)
    print("wrote", out)


def main():
    os.makedirs(FIGS, exist_ok=True)
    layer = Layer("hdbscan")
    # The pickle is this probe's own output, written by `measure.py` in this directory.
    with open(os.path.join(HERE, "rings-hdbscan.pkl"), "rb") as fh:
        rings_by_key = pickle.load(fh)
    corpus = corpus_positions()

    for family, name in [
        ("convex", "Convex wrap — every artifact of clusters/hdbscan"),
        ("dig", "Alpha shape as built on main — every artifact"),
        ("chi", "Chi-shape at the same alpha — every artifact"),
        ("alpha_complex", "Alpha-complex at the same alpha — every artifact"),
    ]:
        layer_figure(layer, rings_by_key, family, corpus, name)

    for key in ["hdb-2422544", "hdb-2422489", "hdb-2422728"]:
        p = np.unique(layer.members(layer.keys.index(key)).astype(np.float64), axis=0)
        families_figure(p, key)

    side_by_side(layer, ["hdb-2422632", "hdb-2422644", "hdb-2422716", "hdb-2422728", "hdb-2422489", "hdb-2422544"])

    alpha_key = "hdb-2422489"
    alpha_figure(np.unique(layer.members(layer.keys.index(alpha_key)).astype(np.float64), axis=0), alpha_key)
    multimodal_figure(layer, ["hdb-2422491", "hdb-2422523", "hdb-2422544"])


if __name__ == "__main__":
    main()
