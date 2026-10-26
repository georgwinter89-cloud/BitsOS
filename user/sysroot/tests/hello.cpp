// C++ smoke test for the V2 sysroot: iostream, std::thread + mutex, exceptions across a thread and
// through several frames with destructors, condition_variable (futex path), RTTI, and from_chars for
// double (the patched charconv.cpp) including malformed input.
#include <atomic>
#include <charconv>
#include <condition_variable>
#include <iostream>
#include <mutex>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>

static int risky(int n) {
  if (n == 3) throw std::runtime_error("three");
  return n * n;
}

static int destroyed = 0;
struct Guard {
  ~Guard() { ++destroyed; }
};
// Each level owns a Guard, so unwinding must run a destructor in every frame on the way out.
static int deep(int n) {
  Guard g;
  if (n == 0) throw std::invalid_argument("deep");
  return deep(n - 1) + 1;
}

static bool bad_parse(const char* s) {
  double d = 7;
  std::string str(s);
  auto r = std::from_chars(str.data(), str.data() + str.size(), d);
  return r.ec == std::errc::invalid_argument && r.ptr == str.data() && d == 7;
}

int main() {
  std::atomic<int> sum{0};
  std::mutex m;
  std::vector<std::string> caught;
  std::vector<std::thread> ts;
  for (int i = 0; i < 6; ++i)
    ts.emplace_back([&, i] {
      try {
        sum += risky(i);
      } catch (const std::exception& e) {
        std::lock_guard<std::mutex> g(m);
        caught.push_back(e.what());
      }
    });
  for (auto& t : ts) t.join();

  // Handshake: the worker waits for "ping", answers "pong"; main waits for the answer.
  std::condition_variable cv;
  int stage = 0;
  std::thread worker([&] {
    std::unique_lock<std::mutex> lk(m);
    cv.wait(lk, [&] { return stage == 1; });
    stage = 2;
    lk.unlock();
    cv.notify_one();
  });
  {
    std::unique_lock<std::mutex> lk(m);
    stage = 1;
    cv.notify_one();
    cv.wait(lk, [&] { return stage == 2; });
  }
  worker.join();

  std::string deep_what;
  try {
    deep(5);
  } catch (const std::invalid_argument& e) {
    deep_what = e.what();
  }

  double d = 0;
  const char s[] = "2.5e3";
  auto r = std::from_chars(s, s + sizeof s - 1, d);
  bool bad_ok = bad_parse("") && bad_parse("abc") && bad_parse("-") && bad_parse(".e5");

  try {
    throw std::out_of_range("main");
  } catch (const std::logic_error& e) {
    std::cout << "caught " << typeid(e).name() << " (" << e.what() << ")\n";
  }
  std::cout << "hello from C++ on musl: threads=" << ts.size() << " sum=" << sum.load()
            << " caught=" << caught.size() << ":" << (caught.empty() ? "" : caught[0])
            << " from_chars=" << d << (r.ec == std::errc{} ? " ok" : " bad") << "\n";
  std::cout << "cv=" << stage << " deep=" << deep_what << " dtors=" << destroyed
            << " bad_input=" << (bad_ok ? "rejected" : "WRONG") << std::endl;
  return 0;
}
