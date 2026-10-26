// AB-2: a C++ program on libc++ (UL-1's sysroot) without threads (clone comes with AB-4): iostream, strings,
// containers, exceptions through frames with destructors, RTTI, static constructors, floating point output.
#include <algorithm>
#include <cstdio>
#include <iostream>
#include <map>
#include <memory>
#include <sstream>
#include <stdexcept>
#include <string>
#include <typeinfo>
#include <vector>

static int destroyed = 0;
struct Guard {
  int n;
  ~Guard() { destroyed += n; }
};
struct Base { virtual ~Base() = default; virtual const char *who() const { return "base"; } };
struct Derived : Base { const char *who() const override { return "derived"; } };

static bool is_derived(const Base &r) { return typeid(r) == typeid(Derived); }

static std::string early = [] { return std::string("static constructor ran"); }();

static int dive(int depth) {
  Guard g{depth};
  if (depth == 0) throw std::runtime_error("deep error");
  return dive(depth - 1) + 1;
}

int main(int argc, char **argv) {
  std::cout << "hello from C++ (libc++) on " << (argc > 1 ? argv[1] : "?") << '\n';
  std::cout << early << '\n';
  std::vector<int> v{5, 3, 9, 1, 7};
  std::sort(v.begin(), v.end());
  std::ostringstream os;
  for (int x : v) os << x << ' ';
  std::cout << "sorted: " << os.str() << '\n';
  std::map<std::string, int> m{{"b", 2}, {"a", 1}, {"c", 3}};
  for (auto &[k, val] : m) std::cout << k << '=' << val << ';';
  std::cout << '\n';
  try {
    dive(6);
  } catch (const std::exception &e) {
    std::cout << "caught: " << e.what() << ", destructors summed " << destroyed << '\n';
  }
  std::unique_ptr<Base> b = std::make_unique<Derived>();
  std::cout << "virtual: " << b->who() << ", rtti derived: " << is_derived(*b) << '\n';
  std::printf("double: %.6f %g\n", 3.14159265358979, 1e-7);
  std::cout << "to_string: " << std::to_string(1234567) << '\n';
  return 0;
}
