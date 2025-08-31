//! The global allocator
use crate::config::KERNEL_HEAP_SIZE;
use buddy_system_allocator::LockedHeap;

#[global_allocator]
/// heap allocator instance
static HEAP_ALLOCATOR: LockedHeap = LockedHeap::empty();

// alloc Crate 是什么：这是 Rust 标准库的一个子集。它包含了所有需要动态内存分配（堆分配）的数据结构，比如 Box, Vec, String, HashMap 等。
// 它的特点：alloc crate 本身并不知道如何分配内存。它只是定义了这些数据结构的行为。当 Box::new() 或 Vec::push() 需要内存时，它们会去调用一个全局的、抽象的内存分配接口。它们只负责“提出请求”（比如，“我需要 16 字节，8 字节对齐的内存”），而不关心谁来“满足请求”。
// GlobalAlloc Trait 是什么：这是一个定义在 core::alloc 中的 unsafe trait。它就是那个抽象的内存分配接口。它规定了任何想要成为“全局分配器”的类型必须实现两个核心方法：
// unsafe trait GlobalAlloc {
//     // 分配内存
//     fn alloc(&self, layout: Layout) -> *mut u8;
//
//     // 释放内存
//     fn dealloc(&self, ptr: *mut u8, layout: Layout);
// }
// 它的作用：它定义了一个契约。任何实现了 GlobalAlloc trait 的类型，都有能力响应来自 alloc crate 的内存请求。在您的代码中，buddy_system_allocator::LockedHeap 这个类型就已经为您实现了这个 trait。它知道如何在一个给定的内存池（也就是 HEAP_SPACE）中使用伙伴系统算法来分配和释放内存块。
// #[global_allocator] 属性是什么：这是一个编译器指令。它就是将前面两者连接起来的**“胶水”或“插头”**。
// 它的作用：当您将这个属性附加到一个 static 变量上时（这个变量的类型必须实现了 GlobalAlloc trait），您在告诉 Rust 编译器：
// “对于整个程序，每当 alloc crate 中的任何代码需要分配或释放内存时，请将调用转发到 HEAP_ALLOCATOR 这个静态实例上！”


#[alloc_error_handler]
/// panic when heap allocation error occurs
pub fn handle_alloc_error(layout: core::alloc::Layout) -> ! {
    panic!("Heap allocation error, layout = {:?}", layout);
}
/// heap space ([u8; KERNEL_HEAP_SIZE])
static mut HEAP_SPACE: [u8; KERNEL_HEAP_SIZE] = [0; KERNEL_HEAP_SIZE];
/// initiate heap allocator
pub fn init_heap() {
    unsafe {
        HEAP_ALLOCATOR
            .lock()
            .init(HEAP_SPACE.as_ptr() as usize, KERNEL_HEAP_SIZE);
    }
}

#[allow(unused)]
pub fn heap_test() {
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    extern "C" {
        fn sbss();
        fn ebss();
    }
    let bss_range = sbss as usize..ebss as usize;
    let a = Box::new(5);
    // Box::new 是 alloc crate 里的函数。
    // 它需要为数字 5 (一个 i32，大小为 4 字节) 在堆上分配空间。
    // alloc crate 的内部代码发出一个抽象的内存分配请求。
    // 因为您在 HEAP_ALLOCATOR 上标注了 #[global_allocator]，编译器自动将这个请求路由到了 HEAP_ALLOCATOR.alloc(...) 方法。
    // HEAP_ALLOCATOR (一个 LockedHeap) 会锁定自己，然后在其内部的伙伴系统数据结构中，从 HEAP_SPACE 静态数组里切出一块合适的内存，并返回一个裸指针。
    // Box::new 拿到这个指针，将数字 5 写入其中，然后将其包装成一个安全的 Box 类型返回。
    assert_eq!(*a, 5);
    assert!(bss_range.contains(&(a.as_ref() as *const _ as usize)));
    drop(a);
    // 当 Box 离开作用域时，它的 drop 方法被调用。
    // drop 方法的实现同样来自 alloc crate。它会发出一个抽象的内存释放请求。
    // 同样，这个请求被编译器路由到 HEAP_ALLOCATOR.dealloc(...) 方法。
    // HEAP_ALLOCATOR 再次锁定自己，将这块内存标记为可用，归还给伙伴系统管理的内存池。
    let mut v: Vec<usize> = Vec::new();
    for i in 0..500 {
        v.push(i);
    }
    for (i, val) in v.iter().take(500).enumerate() {
        assert_eq!(*val, i);
    }
    assert!(bss_range.contains(&(v.as_ptr() as usize)));
    drop(v);
    println!("heap_test passed!");
}
