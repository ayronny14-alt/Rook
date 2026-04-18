#!/usr/bin/env python3
import argparse
import json
import math
import os
import sqlite3
import time
from collections import defaultdict


def clamp(value, low=0.0, high=1.0):
    return max(low, min(high, value))


def normalize(vec):
    norm = math.sqrt(sum(v * v for v in vec))
    if norm <= 1e-12:
        return vec[:]
    return [v / norm for v in vec]


def average(vectors, dims):
    if not vectors:
        return [0.0] * dims
    out = [0.0] * dims
    for vec in vectors:
        for i, value in enumerate(vec[:dims]):
            out[i] += float(value)
    count = float(len(vectors))
    return [v / count for v in out]


def hashed_features(text, dims):
    out = [0.0] * dims
    for idx, ch in enumerate(text.encode('utf-8')):
        out[idx % dims] += ch / 255.0
    return normalize(out)


def cosine(a, b):
    return sum(float(x) * float(y) for x, y in zip(a, b))


def sigmoid(x):
    if x >= 0:
        z = math.exp(-x)
        return 1.0 / (1.0 + z)
    z = math.exp(x)
    return z / (1.0 + z)


def infer_relationship(meta_a, meta_b):
    type_a = (meta_a.get('node_type') or 'concept').lower()
    type_b = (meta_b.get('node_type') or 'concept').lower()
    types = {type_a, type_b}

    if type_a == type_b:
        return 'relates_to'
    if 'file' in types and ('symbol' in types or 'concept' in types):
        return 'references'
    if 'task' in types or 'action' in types:
        return 'supports'
    if 'conversation' in types or 'message' in types:
        return 'mentions'
    if 'error' in types or 'issue' in types:
        return 'causes'
    return 'relates_to'


def build_labels(node_ids, metadata):
    label_names = sorted({(metadata[node_id].get('node_type') or 'concept').lower() for node_id in node_ids})
    label_map = {name: idx for idx, name in enumerate(label_names)}
    labels = [label_map[(metadata[node_id].get('node_type') or 'concept').lower()] for node_id in node_ids]
    return labels, label_names


def load_graph(db_path):
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row

    nodes = conn.execute(
        "SELECT id, node_type, title, created_at, updated_at, metadata_json FROM nodes"
    ).fetchall()
    edges = conn.execute(
        "SELECT source_id, target_id, relationship, strength FROM edges"
    ).fetchall()
    embedding_rows = conn.execute(
        "SELECT node_id, vector_json FROM embeddings"
    ).fetchall()

    feedback_rows = []
    try:
        feedback_rows = conn.execute(
            "SELECT node_id, rating FROM memory_feedback"
        ).fetchall()
    except Exception:
        feedback_rows = []

    feedback_by_node = defaultdict(list)
    for row in feedback_rows:
        try:
            feedback_by_node[row['node_id']].append(float(row['rating']))
        except Exception:
            pass

    grouped_embeddings = defaultdict(list)
    max_dims = 32
    for row in embedding_rows:
        try:
            vec = json.loads(row['vector_json'])
            if isinstance(vec, list) and vec:
                grouped_embeddings[row['node_id']].append([float(v) for v in vec])
                max_dims = max(max_dims, len(vec))
        except Exception:
            pass

    node_ids = []
    node_index = {}
    features = []
    metadata = {}
    for idx, row in enumerate(nodes):
        node_id = row['id']
        node_ids.append(node_id)
        node_index[node_id] = idx
        title = row['title'] or ''
        node_type = row['node_type'] or 'concept'
        seed = f"{node_type} {title}"

        parsed_meta = {}
        raw_meta = row['metadata_json'] or ''
        if raw_meta:
            try:
                parsed_meta = json.loads(raw_meta)
                if not isinstance(parsed_meta, dict):
                    parsed_meta = {}
            except Exception:
                parsed_meta = {}

        feedback_avg = 0.0
        if feedback_by_node.get(node_id):
            feedback_avg = sum(feedback_by_node[node_id]) / float(len(feedback_by_node[node_id]))
            feedback_avg = clamp(feedback_avg / 2.0, -1.0, 1.0)

        if grouped_embeddings.get(node_id):
            feat = average(grouped_embeddings[node_id], max_dims)
        else:
            feat = hashed_features(seed, max_dims)

        if feat:
            feat[0] += 0.12 * feedback_avg
            if len(feat) > 1:
                feat[1] += 0.08 * float(parsed_meta.get('confidence', 0.65))
            if len(feat) > 2:
                feat[2] += 0.05 if node_type.lower() in {'file', 'symbol', 'concept'} else -0.02

        features.append(normalize(feat))
        metadata[node_id] = {
            'title': title,
            'node_type': node_type,
            'updated_at': row['updated_at'] or 0,
            'feedback': feedback_avg,
            'confidence': float(parsed_meta.get('confidence', 0.65)),
        }

    edge_list = []
    adjacency = defaultdict(list)
    edge_weights = {}
    for row in edges:
        src = row['source_id']
        dst = row['target_id']
        if src in node_index and dst in node_index:
            weight = float(row['strength'] or 1.0)
            src_idx = node_index[src]
            dst_idx = node_index[dst]
            edge_list.append((src_idx, dst_idx, weight))
            edge_list.append((dst_idx, src_idx, weight))
            adjacency[src_idx].append((dst_idx, weight))
            adjacency[dst_idx].append((src_idx, weight))
            key = tuple(sorted((src_idx, dst_idx)))
            edge_weights[key] = max(edge_weights.get(key, 0.0), weight)

    return node_ids, features, edge_list, adjacency, metadata, edge_weights


def summarize_predictions(node_ids, embeddings, adjacency, metadata, edge_weights):
    node_confidence = {}
    node_labels = {}
    edge_updates = []
    predicted_edges = []

    for idx, node_id in enumerate(node_ids):
        neighbors = adjacency.get(idx, [])
        if neighbors:
            local_alignment = sum(
                max(0.0, cosine(embeddings[idx], embeddings[neighbor_idx])) * max(0.1, min(weight, 1.5))
                for neighbor_idx, weight in neighbors
            ) / float(len(neighbors))
        else:
            local_alignment = 0.42

        degree_signal = min(1.0, len(neighbors) / 6.0)
        feedback_signal = 0.5 + (0.25 * float(metadata[node_id].get('feedback', 0.0)))
        prior_confidence = clamp(float(metadata[node_id].get('confidence', 0.65)))
        node_confidence[node_id] = clamp(
            0.25 * prior_confidence + 0.35 * local_alignment + 0.20 * degree_signal + 0.20 * feedback_signal
        )
        node_labels[node_id] = (metadata[node_id].get('node_type') or 'concept').lower()

    for (src_idx, dst_idx), prior_weight in edge_weights.items():
        similarity = (cosine(embeddings[src_idx], embeddings[dst_idx]) + 1.0) / 2.0
        source_id = node_ids[src_idx]
        target_id = node_ids[dst_idx]
        feedback_bonus = 0.05 * (metadata[source_id].get('feedback', 0.0) + metadata[target_id].get('feedback', 0.0))
        score = clamp(0.55 * prior_weight + 0.40 * similarity + feedback_bonus)
        edge_updates.append({
            'source_id': source_id,
            'target_id': target_id,
            'relationship': infer_relationship(metadata[source_id], metadata[target_id]),
            'score': round(score, 4),
        })

    total_nodes = len(node_ids)
    for src_idx in range(total_nodes):
        for dst_idx in range(src_idx + 1, total_nodes):
            if (src_idx, dst_idx) in edge_weights:
                continue

            similarity = (cosine(embeddings[src_idx], embeddings[dst_idx]) + 1.0) / 2.0
            shared_neighbors = len(
                {n for n, _ in adjacency.get(src_idx, [])}.intersection({n for n, _ in adjacency.get(dst_idx, [])})
            )
            confidence_boost = (node_confidence[node_ids[src_idx]] + node_confidence[node_ids[dst_idx]]) / 2.0
            score = clamp(0.72 * similarity + 0.18 * min(1.0, shared_neighbors / 3.0) + 0.10 * confidence_boost)
            if score < 0.82:
                continue

            source_id = node_ids[src_idx]
            target_id = node_ids[dst_idx]
            predicted_edges.append({
                'source_id': source_id,
                'target_id': target_id,
                'relationship': infer_relationship(metadata[source_id], metadata[target_id]),
                'score': round(score, 4),
            })

    predicted_edges.sort(key=lambda item: item['score'], reverse=True)
    return node_confidence, node_labels, edge_updates, predicted_edges[:64]


def rule_based_graphsage(node_ids, features, adjacency, metadata, edge_weights, rounds=3):
    current = [vec[:] for vec in features]
    dims = len(current[0]) if current else 32
    for _ in range(rounds):
        next_features = []
        for idx, vec in enumerate(current):
            neighbors = adjacency.get(idx, [])
            if not neighbors:
                next_features.append(normalize(vec))
                continue
            total_weight = sum(max(weight, 0.05) for _, weight in neighbors)
            agg = [0.0] * dims
            for neighbor_idx, weight in neighbors:
                scaled = max(weight, 0.05)
                for dim in range(dims):
                    agg[dim] += current[neighbor_idx][dim] * scaled
            agg = [value / total_weight for value in agg]
            mixed = [(0.58 * vec[d]) + (0.42 * agg[d]) for d in range(dims)]
            next_features.append(normalize(mixed))
        current = next_features

    node_confidence, node_labels, edge_updates, predicted_edges = summarize_predictions(
        node_ids, current, adjacency, metadata, edge_weights
    )
    return current, node_confidence, node_labels, edge_updates, predicted_edges, 'rule-based-self-supervised'


def try_pyg_graphsage(node_ids, features, edge_list, adjacency, metadata, edge_weights, epochs, hidden):
    try:
        import torch
        import torch.nn as nn
        import torch.nn.functional as F
        from torch_geometric.data import Data
        from torch_geometric.nn import GraphSAGE
    except Exception as exc:
        return None, None, None, None, None, f"pyg-unavailable: {exc}"

    if not features:
        return [], {}, {}, [], [], 'empty-graph'

    labels, label_names = build_labels(node_ids, metadata)
    x = torch.tensor(features, dtype=torch.float)
    y = torch.tensor(labels, dtype=torch.long)

    if edge_list:
        edge_index = torch.tensor([[s, d] for s, d, _ in edge_list], dtype=torch.long).t().contiguous()
    else:
        edge_index = torch.empty((2, 0), dtype=torch.long)

    data = Data(x=x, edge_index=edge_index)
    in_dim = x.size(1)
    hidden_dim = min(hidden, max(64, in_dim))
    num_classes = max(1, len(label_names))

    class GraphSageTrainer(nn.Module):
        def __init__(self, in_channels, hidden_channels, classes):
            super().__init__()
            self.encoder = GraphSAGE(
                in_channels=in_channels,
                hidden_channels=hidden_channels,
                num_layers=2,
                out_channels=hidden_channels,
                dropout=0.10,
            )
            self.reconstructor = nn.Linear(hidden_channels, in_channels)
            self.classifier = nn.Linear(hidden_channels, classes)

        def forward(self, x_in, edges_in):
            z = self.encoder(x_in, edges_in)
            recon = self.reconstructor(z)
            logits = self.classifier(z)
            return z, recon, logits

    model = GraphSageTrainer(in_dim, hidden_dim, num_classes)
    optimizer = torch.optim.Adam(model.parameters(), lr=0.008, weight_decay=1e-4)

    for _ in range(max(10, epochs)):
        model.train()
        optimizer.zero_grad()
        z, recon, logits = model(data.x, data.edge_index)

        recon_loss = F.mse_loss(recon, data.x)
        class_loss = F.cross_entropy(logits, y) if num_classes > 1 else torch.tensor(0.0)

        if edge_index.numel() > 0:
            src, dst = edge_index
            pos_logits = (z[src] * z[dst]).sum(dim=1)
            neg_dst = torch.randint(0, data.num_nodes, (src.size(0),), dtype=torch.long)
            neg_logits = (z[src] * z[neg_dst]).sum(dim=1)
            link_loss = F.binary_cross_entropy_with_logits(pos_logits, torch.ones_like(pos_logits))
            link_loss = link_loss + F.binary_cross_entropy_with_logits(neg_logits, torch.zeros_like(neg_logits))
        else:
            link_loss = torch.tensor(0.0)

        noise = torch.randn_like(data.x) * 0.01
        z_view, _, _ = model(data.x + noise, data.edge_index)
        contrastive_loss = 1.0 - F.cosine_similarity(z, z_view, dim=1).mean()

        loss = 0.30 * recon_loss + 0.35 * link_loss + 0.25 * class_loss + 0.10 * contrastive_loss
        loss.backward()
        optimizer.step()

    model.eval()
    with torch.no_grad():
        z, _, logits = model(data.x, data.edge_index)
        embeddings = [normalize([float(v) for v in row]) for row in z.cpu().tolist()]
        if num_classes > 1:
            probs = F.softmax(logits, dim=1)
            confidence_values, predicted_idx = torch.max(probs, dim=1)
            node_labels = {
                node_ids[idx]: label_names[int(predicted_idx[idx].item())]
                for idx in range(len(node_ids))
            }
            base_confidence = {
                node_ids[idx]: float(confidence_values[idx].item())
                for idx in range(len(node_ids))
            }
        else:
            node_labels = {node_id: label_names[0] if label_names else 'concept' for node_id in node_ids}
            base_confidence = {node_id: 0.65 for node_id in node_ids}

    extra_confidence, _, edge_updates, predicted_edges = summarize_predictions(
        node_ids, embeddings, adjacency, metadata, edge_weights
    )
    node_confidence = {
        node_id: round(clamp(0.55 * base_confidence.get(node_id, 0.65) + 0.45 * extra_confidence.get(node_id, 0.65)), 4)
        for node_id in node_ids
    }
    return embeddings, node_confidence, node_labels, edge_updates, predicted_edges, 'trained-pyg-self-supervised'


def main():
    parser = argparse.ArgumentParser(description='Train or approximate GraphSAGE node embeddings for DocuBot.')
    parser.add_argument('--db', required=True, help='Path to DocuBot SQLite memory database')
    parser.add_argument('--out', required=True, help='Output JSON artifact path')
    parser.add_argument('--epochs', type=int, default=60)
    parser.add_argument('--hidden', type=int, default=128)
    parser.add_argument('--min-nodes', type=int, default=25)
    args = parser.parse_args()

    node_ids, features, edge_list, adjacency, metadata, edge_weights = load_graph(args.db)
    if len(node_ids) < args.min_nodes:
        print(f'Not enough nodes to train GraphSAGE yet ({len(node_ids)} < {args.min_nodes})')
        return 0

    trained, node_confidence, node_labels, edge_updates, predicted_edges, strategy = try_pyg_graphsage(
        node_ids,
        features,
        edge_list,
        adjacency,
        metadata,
        edge_weights,
        args.epochs,
        args.hidden,
    )
    if trained is None:
        trained, node_confidence, node_labels, edge_updates, predicted_edges, fallback_strategy = rule_based_graphsage(
            node_ids,
            features,
            adjacency,
            metadata,
            edge_weights,
            rounds=3,
        )
        strategy = f'rule-based-fallback ({fallback_strategy})'

    artifact = {
        'generated_at': int(time.time()),
        'model': 'graphsage',
        'strategy': strategy,
        'node_embeddings': {node_id: vec for node_id, vec in zip(node_ids, trained)},
        'node_confidence': node_confidence,
        'node_labels': node_labels,
        'edge_updates': edge_updates,
        'predicted_edges': predicted_edges,
        'stats': {
            'nodes': len(node_ids),
            'edges': len(edge_list) // 2,
            'predicted_edges': len(predicted_edges),
        },
    }

    out_dir = os.path.dirname(args.out)
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)
    with open(args.out, 'w', encoding='utf-8') as f:
        json.dump(artifact, f)

    print(json.dumps({
        'ok': True,
        'strategy': strategy,
        'nodes': len(node_ids),
        'edges': len(edge_list) // 2,
        'predicted_edges': len(predicted_edges),
        'out': args.out,
    }))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
