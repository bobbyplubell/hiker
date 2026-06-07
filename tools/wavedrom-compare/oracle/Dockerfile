# Oracle container: real wavedrom.js (via wavedrom-cli), the reference renderer
# the pure-Rust `hiker-wavedrom` is compared against. Built once, then run
# per-fixture by `run.sh`. Keeping wavedrom.js in a container honors the
# project's "never run reference JS on the host" rule and pins a known
# wavedrom-cli version so the oracle is reproducible.
FROM node:22-alpine

WORKDIR /oracle

# Install deps first for layer caching; package.json pins the wavedrom-cli
# version. `npm link` exposes the `wavedrom-cli` binary on PATH.
COPY package.json ./
RUN npm install --omit=dev --no-audit --no-fund \
    && ln -s /oracle/node_modules/.bin/wavedrom-cli /usr/local/bin/wavedrom-cli

COPY entrypoint.sh /oracle/entrypoint.sh
RUN chmod +x /oracle/entrypoint.sh

# Fixture WaveJSON arrives on stdin (no bind mount); SVG comes back on stdout.
ENTRYPOINT ["/oracle/entrypoint.sh"]
