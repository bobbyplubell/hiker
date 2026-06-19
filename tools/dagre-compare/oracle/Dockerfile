# Oracle container: real @dagrejs/dagre, the JS layout engine the Rust port was
# ported from. Built once, then run per-fixture by `run.sh`. Keeping dagre.js in
# a container honors the port's rule of never executing the reference JS on the
# host — and pins a known dagre version so the oracle is reproducible.
FROM node:22-alpine

WORKDIR /oracle

# Install deps first for layer caching; package.json pins the dagre version.
COPY package.json ./
RUN npm install --omit=dev --no-audit --no-fund

# debug-ranks.js is a manual helper: per-pass rank dumps for pinning a port
# divergence to a specific pass (see its header for usage).
COPY run.js debug-ranks.js ./

# Fixtures are bind-mounted at /fixtures by run.sh; the fixture path is passed
# as the argument.
ENTRYPOINT ["node", "/oracle/run.js"]
