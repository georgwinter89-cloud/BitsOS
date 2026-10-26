// UL-5: a dynamic C++ program against CachyOS's glibc and libstdc++ (built by xtask with the host's g++). It
// touches each library of the set: exceptions unwind through libgcc_s, iostreams and std::string come from
// libstdc++, std::thread and std::mutex sit on glibc's pthreads, the maths from libm. The transcript is
// deterministic and equal on CachyOS and V2 (pkg-cxx.out; ktest ul.pkg.cxx, xtask test package::tests).
#include <algorithm>
#include <atomic>
#include <cmath>
#include <cstdio>
#include <exception>
#include <iomanip>
#include <map>
#include <memory>
#include <mutex>
#include <sstream>
#include <stdexcept>
#include <string>
#include <thread>
#include <typeinfo>
#include <vector>

struct Shape {
    virtual ~Shape() = default;
    virtual double area() const = 0;
};
struct Circle : Shape {
    double r;
    explicit Circle(double r) : r(r) {}
    double area() const override { return std::acos(-1.0) * r * r; }
};

[[gnu::noinline]] static int deep(int n) {
    std::string keep = "frame " + std::to_string(n);
    if (n == 0) throw std::runtime_error("thrown five frames down");
    return deep(n - 1) + static_cast<int>(keep.size());
}

thread_local int per_thread = 7;

int main() {
    try {
        deep(5);
    } catch (const std::runtime_error &e) {
        std::printf("exception: %s\n", e.what());
    }

    std::ostringstream os;
    os << "stream: " << std::fixed << std::setprecision(6) << std::acos(-1.0) << " " << std::hex << 48879;
    std::printf("%s\n", os.str().c_str());

    volatile double x = 2.0;
    std::printf("libm: %.12f %.12f %.12f %.12f\n", std::sqrt(x), std::exp(x), std::log(x * 5), std::cbrt(x * 13.5));
    std::printf("libm: %.12f %.12f\n", std::pow(x, 0.5), std::sin(x) + std::cos(x));

    std::atomic<long> sum{0};
    std::mutex m;
    std::map<int, int> seen;
    std::vector<std::thread> ts;
    for (int t = 0; t < 4; t++) {
        ts.emplace_back([&, t] {
            per_thread += t;
            for (int i = 1; i <= 1000; i++) sum += i;
            std::lock_guard<std::mutex> g(m);
            seen[t] = per_thread;
        });
    }
    for (auto &t : ts) t.join();
    std::printf("threads: sum=%ld", sum.load());
    for (auto &kv : seen) std::printf(" %d:%d", kv.first, kv.second);
    std::printf(" main:%d\n", per_thread);

    std::exception_ptr ep;
    std::thread([&] {
        try {
            throw std::out_of_range("from a thread");
        } catch (...) {
            ep = std::current_exception();
        }
    }).join();
    try {
        std::rethrow_exception(ep);
    } catch (const std::logic_error &e) {
        std::printf("rethrown: %s (%s)\n", e.what(), typeid(e).name());
    }

    std::vector<std::string> words{"pear", "apple", "fig", "banana"};
    std::sort(words.begin(), words.end());
    std::printf("sorted:");
    for (auto &w : words) std::printf(" %s", w.c_str());
    std::printf("\n");

    std::unique_ptr<Shape> s = std::make_unique<Circle>(2.0);
    std::printf("virtual: %s %.6f %d\n", typeid(*s).name(), s->area(), dynamic_cast<Circle *>(s.get()) != nullptr);
    std::printf("done\n");
    return 0;
}
