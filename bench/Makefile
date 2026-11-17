.PHONY: all capture report

all: capture

capture:
	./run.sh

report:
	python3 scripts/aggregate.py --results results --output results/comparison.md
