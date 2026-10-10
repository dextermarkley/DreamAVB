# Host-side unit tests for the dexts' pure helpers (no DriverKit needed).
# Usage: make -C driver/tests test
# Build output goes to the gitignored agent scratch dir, never /tmp.

ROOT     := $(abspath $(CURDIR)/../..)
OUT      ?= $(ROOT)/.agents/tmp/driver-tests
CXX      := xcrun clang++
CXXFLAGS := -std=c++20 -Wall -Wextra -Werror -O1

.PHONY: test clean

$(OUT)/tg3logic_tests: tg3logic_tests.cpp ../AVBBcmDriver/Tg3Logic.hpp ../AVBBcmDriver/Bcm57762Regs.h ../AVBShared.h
	@mkdir -p "$(OUT)"
	TMPDIR="$(OUT)" $(CXX) $(CXXFLAGS) -o "$@" tg3logic_tests.cpp

$(OUT)/i210logic_tests: i210logic_tests.cpp ../AVBI210Driver/I210Logic.hpp ../AVBI210Driver/I210Regs.h ../AVBShared.h
	@mkdir -p "$(OUT)"
	TMPDIR="$(OUT)" $(CXX) $(CXXFLAGS) -o "$@" i210logic_tests.cpp

test: $(OUT)/tg3logic_tests $(OUT)/i210logic_tests
	"$(OUT)/tg3logic_tests"
	"$(OUT)/i210logic_tests"

clean:
	rm -f "$(OUT)/tg3logic_tests" "$(OUT)/i210logic_tests"
