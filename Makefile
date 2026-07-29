# Run `make help` for the target list. WHICH test runs is a parameter (SUITE=),
# not a target per suite. Override any variable below on the command line.

VERSION        ?= $(shell git rev-parse --short HEAD 2>/dev/null || echo dev)
IMAGE_REGISTRY ?= msb
CLUSTER_NAME   ?= msb-dev

# Override for k3d (CLUSTER_LOAD='k3d image import') or a registry flow.
CLUSTER_LOAD   ?= kind load docker-image --name $(CLUSTER_NAME)

GATEWAY_NS  ?= msb-helm
GATEWAY_URL ?= http://msb-gateway.$(GATEWAY_NS).svc:8080
E2E_SA      ?= sdk-e2e-client

SUITE ?= unit

# Every operator image is docker/<name>/Dockerfile, built the same way.
IMAGES := msb-controller msb-daemon msb-bridge msb-gateway msb-runtime sdk-gateway-e2e

.PHONY: help
help: ## Show this help
	@grep -hE '^[a-zA-Z0-9_%-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-22s\033[0m %s\n", $$1, $$2}'

# --- build ---------------------------------------------------------------------

.PHONY: build
build: ## Build all workspace binaries (release)
	cargo build --release --workspace

image: $(addprefix image-,$(IMAGES)) ## Build all operator images

image-%: ## Build one image: docker/<name>/Dockerfile -> $(IMAGE_REGISTRY)/<name>:$(VERSION)
	docker build -t $(IMAGE_REGISTRY)/$*:$(VERSION) -f docker/$*/Dockerfile .

cluster-load-%: image-% ## Build + load one image into the cluster
	$(CLUSTER_LOAD) $(IMAGE_REGISTRY)/$*:$(VERSION)

# --- test ----------------------------------------------------------------------

.PHONY: test
test: test-$(SUITE) ## Run a test suite: make test SUITE=unit|e2e|e2e-sdk

.PHONY: test-unit
test-unit: ## Hermetic unit tests, no cluster
	cargo test --all

.PHONY: test-e2e
test-e2e: ## KVM kuttl suite (needs KUBECONFIG at a KVM cluster)
	tests/e2e/run.sh

.PHONY: test-e2e-sdk
test-e2e-sdk: cluster-load-sdk-gateway-e2e ## e2e via the unmodified SDK + gateway, in-cluster
	kubectl -n $(GATEWAY_NS) delete pod sdk-gateway-e2e --ignore-not-found
	kubectl -n $(GATEWAY_NS) run sdk-gateway-e2e \
		--image=$(IMAGE_REGISTRY)/sdk-gateway-e2e:$(VERSION) --image-pull-policy=IfNotPresent \
		--restart=Never --attach --rm \
		--overrides='{"spec":{"serviceAccountName":"$(E2E_SA)"}}' \
		--env=MSB_API_URL=$(GATEWAY_URL) \
		--env=MSB_API_KEY=$$(kubectl -n $(GATEWAY_NS) create token $(E2E_SA) --duration=1h)
