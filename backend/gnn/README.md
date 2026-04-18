# DocuBot GraphSAGE Trainer

This folder contains the optional GraphSAGE training pipeline for DocuBot's memory graph.

## Default behavior
- If `torch` + `torch-geometric` are installed, `train_graphsage.py` trains a small GraphSAGE model over the current graph.
- If those libraries are not installed yet, the script falls back to a deterministic neighborhood-smoothing export so the backend still gets graph-aware vectors.

## Usage

```bash
python train_graphsage.py --db ../memory.db --out ../graphsage_embeddings.json
```

## Recommended install

```bash
pip install -r requirements.txt
```

## Notes
- There is **not** a generally useful pretrained GraphSAGE model for DocuBot's private graph schema.
- Use pretrained sentence/local embedding models for node features, then train GraphSAGE on your own graph once enough nodes/edges accumulate.
