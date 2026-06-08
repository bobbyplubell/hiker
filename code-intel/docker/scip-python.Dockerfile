# scip-python (Sourcegraph's Python SCIP indexer, built on pyright).
# Node stays quarantined in this container — never on the host.
# Needs python+pip inside the container: scip-python shells out to `pip list`
# to evaluate the project's environment/dependencies.
FROM node:22-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 python3-pip \
 && rm -rf /var/lib/apt/lists/*
RUN pip3 install --no-cache-dir --break-system-packages pytest
RUN npm install -g @sourcegraph/scip-python@latest
WORKDIR /work
ENTRYPOINT ["scip-python"]
