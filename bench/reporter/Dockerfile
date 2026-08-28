ARG PYTHON_IMAGE=docker.io/library/python:3.13.15-slim-bookworm@sha256:c45a22ea000adfd9cda29364bbe7edd23001ce5cc2ad15857cfbf7766943b9ca
FROM ${PYTHON_IMAGE}

ARG MATPLOTLIB_VERSION
COPY requirements.txt /tmp/requirements.txt
RUN apt-get update \
    && apt-get install -y --no-install-recommends openssl \
    && rm -rf /var/lib/apt/lists/* \
    && sed -i "s/^matplotlib==.*/matplotlib==${MATPLOTLIB_VERSION}/" /tmp/requirements.txt \
    && pip install --no-cache-dir --disable-pip-version-check -r /tmp/requirements.txt \
    && rm -f /tmp/requirements.txt

ENV MPLBACKEND=Agg
