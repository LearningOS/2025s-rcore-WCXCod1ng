use alloc::vec;
use alloc::vec::Vec;

/// 检查系统是否处于安全状态（银行家算法的核心）
pub fn is_safe(allocation: &Vec<Vec<usize>>, available: & Vec<usize>, need: &Vec<Vec<usize>>) -> bool {
    let n = allocation.len();
    if n == 0 {
        return true;
    }
    let m = allocation[0].len();

    let mut work = available.clone();
    let mut finish = vec![false; n];
    loop {
        // 2.1 检查满足条件的线程
        if let Some(idx) = (0..n).find(|i| {
            !finish[*i] && (0..m).all(|j| { need[*i][j] <= work[j] })
        }) {
            // 2.2 找到一个这样的线程，为其分配，执行完毕后就可以回收资源。并为下一个线程分配
            // 现在为线程idx分配
            for j in 0..m {
                work[j] += allocation[idx][j];
            }
            finish[idx] = true;
        } else {
            // 2.3 找不到这样的一个线程，需要停止
            // 如果有一个没有完成，则为不安全状态
            if finish.iter().any(|x| *x == false) {
                debug!("finish = {:?}", &finish);
                return false;
            } else {
                break; // 否则说明都顺利完成了，处于安全状态，可以分配
            }
        }
    }
    true
}