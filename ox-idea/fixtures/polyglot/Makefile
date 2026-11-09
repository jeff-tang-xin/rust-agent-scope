.PHONY: all api web worker native test clean

all: api native

api:
	cd api && go build -o ../bin/api .

native:
	mkdir -p bin && cc -O2 -o bin/hash native/hash.c

worker:
	python3 worker/worker.py --once

test:
	cd api && go test ./...
	python3 -m pytest worker

clean:
	rm -rf bin
