ARG PYTHON_IMAGE
FROM ${PYTHON_IMAGE}

ARG MATPLOTLIB_VERSION
COPY requirements.txt /tmp/requirements.txt
RUN sed -i "s/^matplotlib==.*/matplotlib==${MATPLOTLIB_VERSION}/" /tmp/requirements.txt \
    && pip install --no-cache-dir --disable-pip-version-check -r /tmp/requirements.txt \
    && rm -f /tmp/requirements.txt

ENV MPLBACKEND=Agg
