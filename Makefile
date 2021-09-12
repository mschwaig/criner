.PHONY : tests build

help:  ## Display this help
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage:\n  make \033[36m<target>\033[0m\n"} /^[a-zA-Z_-]+:.*?##/ { printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2 } /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) } ' $(MAKEFILE_LIST)


EXECUTABLE = target/debug/criner
RELEASE_EXECUTABLE = target/release/criner
RUST_SRC_FILES = $(shell find src criner/src -name "*.rs") Cargo.lock
bare_index_path = index-bare

DB = criner.db
SQLITE_DB = $(DB)/db.msgpack.sqlite
REPORTS = $(DB)/reports
WASTE_REPORT = $(REPORTS)/waste

$(bare_index_path):
	mkdir -p $(dir $@)
	git clone --bare https://github.com/rust-lang/crates.io-index $@

$(EXECUTABLE): $(RUST_SRC_FILES)
	cargo build

$(RELEASE_EXECUTABLE): $(RUST_SRC_FILES)
	cargo build --release

##@ Meta

nix-shell-macos: ## Enter a nix-shell able to build on macos
	nix-shell -p pkg-config openssl libiconv darwin.apple_sdk.frameworks.Security darwin.apple_sdk.frameworks.SystemConfiguration zlib

sloc: ## Count lines of code, without tests
	tokei -e '*_test*'

##@ Running Criner

$(WASTE_REPORT):
		mkdir -p $(REPORTS)
		git clone https://github.com/the-lean-crate/waste $@

init: $(WASTE_REPORT) ## Clone output repositories for report generation. Only needed if you have write permissions to https://github.com/crates-io
fetch-only: $(RELEASE_EXECUTABLE) $(bare_index_path) ## Run the fetch stage once
		$(RELEASE_EXECUTABLE) mine -c $(bare_index_path) -F 1 -P 0 -R 0 $(DB)
process-only: $(RELEASE_EXECUTABLE) $(bare_index_path) ## Run the processing stage once
		$(RELEASE_EXECUTABLE) mine -c $(bare_index_path) --io 10 --cpu 2  -F 0 -P 1 -R 0 $(DB)
process-only-nonstop: $(RELEASE_EXECUTABLE) $(bare_index_path) ## Run the processing stage continuously
		$(RELEASE_EXECUTABLE) mine -c $(bare_index_path) --io 10 --cpu 2  -F 0 -p 5min -R 0 $(DB)
report-only: $(RELEASE_EXECUTABLE) $(bare_index_path) ## Run the reporting stage once
		$(RELEASE_EXECUTABLE) mine -c $(bare_index_path) --cpu-o 10  -F 0 -P 0 -R 1 $(DB)
force-report-only: $(RELEASE_EXECUTABLE) $(bare_index_path) ## Run the reporting stage once, forcibly, rewriting everything and ignoring caches
		$(RELEASE_EXECUTABLE) mine -c $(bare_index_path) --cpu-o 10  -F 0 -P 0 -R 1 -g '*' $(DB)
mine-nonstop: $(RELEASE_EXECUTABLE) $(bare_index_path) ## Run all operations continuously, fully automated
		ulimit -n 512; $(RELEASE_EXECUTABLE) mine -c $(bare_index_path) --io 10 --cpu 1 --cpu-o 10 -d 3:00 $(DB)
mine-nonstop-no-report: $(RELEASE_EXECUTABLE) $(bare_index_path) ## Run all operations continuously, fully automated
		ulimit -n 512; $(RELEASE_EXECUTABLE) mine -c $(bare_index_path) --io 10 --cpu 1 --cpu-o 10 -d 3:00 -R 0 $(DB)
mine-nonstop-logonly: $(RELEASE_EXECUTABLE) $(bare_index_path) ## Run all operations continuously, fully automated, without gui
		ulimit -n 512; $(RELEASE_EXECUTABLE) mine --no-gui -c $(bare_index_path) --io 10 --cpu 1 --cpu-o 10 $(DB)
mine-2min-logonly: $(RELEASE_EXECUTABLE) $(bare_index_path) ## Run all operations continuously, painfully often, and for two minutes only
		ulimit -n 512; $(RELEASE_EXECUTABLE) mine --time-limit 2min --no-gui -c $(bare_index_path) --io 10 --cpu 1 --cpu-o 10 -f 10s -p 10s -r 10s $(DB)

##@ Waste Report Maintenance

waste-report-push-changes: $(WASTE_REPORT) ## add, commit and push all changed report pages
		cd $(WASTE_REPORT) && git add . && git commit -m "update" && git push origin +HEAD:master

waste-report-reset-history-and-push: $(WASTE_REPORT) ## clear the history of the waste report repository to reduce its size, and push everything
		cd $(WASTE_REPORT); git checkout -b foo; git branch -D tmp; git checkout --orphan tmp; git branch -D foo;
		$(MAKE) waste-report-push-changes;

waste-report-clear-state: $(SQLITE_DB) $(WASTE_REPORT) ## clear database state and local state for waste reporting, but leave all html files
		-sqlite3 $< 'drop table report_done;'
		-rm -Rf $(WASTE_REPORT)/__incremental_cache__

##@ Testing

clippy: ## Run cargo clippy
	cargo clippy

fmt: ## Run cargo fmt in check mode
	cargo fmt --all -- --check

tests: fmt clippy ## Run all tests we have
	cargo check --all --tests
	cd criner-waste-report && cargo check --tests && cargo check --tests --no-default-features
	cargo test --all

##@ Dataset

crates-io-db-dump.tar.gz:
	curl --progress https://static.crates.io/db-dump.tar.gz > $@

update-crate-db: crates-io-db-dump.tar.gz ## Pull all DB data from crates.io - updated every 24h

