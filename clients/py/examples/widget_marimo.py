# The same example as widget.ipynb, as a Marimo notebook: `marimo edit --port 5173 clients/py/examples/widget_marimo.py`.
# Marimo folds every synced trait into one `.value`, so the reading cell re-runs at every settle.
import marimo

__generated_with = "0.24.0"
app = marimo.App()


@app.cell
def _():
    import json
    import os
    import pathlib

    import marimo as mo
    import tesseradb

    # What the demo is serving right now: `run_demo.sh` writes it under `tessera-demo/` at the
    # checkout root (gitignored), presets included.
    demo = json.loads((pathlib.Path(os.environ.get("TESSERA_DEMO_DIR", "../../../tessera-demo")) / "datasets.json").read_text())["datasets"][0]
    preset = next(p for p in demo["presets"] if p["label"].startswith("medium"))
    VIEWER = demo["viewerUrl"]
    # Operator-only: the credential mints any principal; the widget gets the token, never the credential.
    token = tesseradb.authorise(demo["sessionUrl"], "dev-session-credential", preset["terms"])
    return mo, tesseradb, token, VIEWER


@app.cell
def _(mo, tesseradb, token, VIEWER):
    m = mo.ui.anywidget(
        tesseradb.Map(
            VIEWER,
            token=token,
            layers=["clusters/kmeans-mt8hcvlg"],
            colour_by="cluster:clusters/kmeans-mt8hcvlg",
            height=520,
        )
    )
    m
    return (m,)


@app.cell
def _(m):
    # Re-runs at every settle: `.value` is every synced trait.
    {k: m.value.get(k) for k in ("bbox", "layers", "colour_by", "selected", "selected_artifact", "region")}
    return


if __name__ == "__main__":
    app.run()
