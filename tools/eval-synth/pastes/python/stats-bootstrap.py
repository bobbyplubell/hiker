import random
from statistics import mean, stdev


def bootstrap_ci(samples, n_iter=10_000, alpha=0.05, seed=0):
    rng = random.Random(seed)
    n = len(samples)
    means = []
    for _ in range(n_iter):
        resample = [samples[rng.randrange(n)] for _ in range(n)]
        means.append(mean(resample))
    means.sort()
    lo = means[int(n_iter * alpha / 2)]
    hi = means[int(n_iter * (1 - alpha / 2))]
    return mean(samples), (lo, hi), stdev(samples)
