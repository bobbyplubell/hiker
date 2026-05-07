def embed_batches(texts, model, batch_size=64):
    embeddings = []
    for i in range(0, len(texts), batch_size):
        chunk = texts[i:i + batch_size]
        out = model.embed(chunk)
        if len(out) != len(chunk):
            raise RuntimeError(f"embedder returned {len(out)} for batch of {len(chunk)}")
        embeddings.extend(out)
    return embeddings


def cosine(a, b):
    dot = sum(x * y for x, y in zip(a, b))
    na = sum(x * x for x in a) ** 0.5
    nb = sum(y * y for y in b) ** 0.5
    return dot / (na * nb) if na and nb else 0.0
