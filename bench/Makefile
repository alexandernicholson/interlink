.PHONY: all setup interlink linkerd istio report teardown local

all: interlink linkerd istio report

setup:
	./setup.sh

interlink:
	./scripts/run-interlink.sh

linkerd:
	./scripts/run-linkerd.sh

istio:
	./scripts/run-istio.sh

report:
	@echo "Aggregating results..."
	python3 scripts/aggregate.py

local:
	cd local && ./run.sh

teardown:
	./teardown.sh
