# Checks v2-toolchain.cmake: the same two programs through CMake + Ninja.
cmake_minimum_required(VERSION 3.20)
project(v2_sysroot_smoke C CXX)
find_package(Threads REQUIRED)
add_executable(hello-c ../hello.c)
add_executable(hello-cpp ../hello.cpp)
target_link_libraries(hello-cpp Threads::Threads)
