CARGO ?= cargo

# Colors for help output
YELLOW := $(shell tput setaf 3)
GREEN  := $(shell tput setaf 2)
RESET  := $(shell tput sgr0)

.PHONY: all help build debug test clean

# Default to release build
all: build

## Help: Show this help message
help:
	@echo "$(YELLOW)Quill Core Build System (Native)$(RESET)"
	@echo "Usage: make [target]"
	@echo ""
	@echo "$(GREEN)Targets:$(RESET)"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  $(YELLOW)%-12s$(RESET) %s\n", $$1, $$2}'

build: ## Build for production in release mode
	@./scripts/build.sh --release

debug: ## Build for development in debug mode
	@./scripts/build.sh

test: ## Run unit and integration tests
	$(CARGO) test

clean: ## Clean cargo artifacts and caches
	$(CARGO) clean
	rm -rf bin/
