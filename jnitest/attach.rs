/// `jint AttachCurrentThread(JavaVM*, void** penv, void* args)`
///
/// native 库自己起的线程要调 Java 就必须先 attach。这里给它 fork 一个 `Vm`
/// （共享同一个 `Shared`：堆/类表/监视器都是同一份），再准备好 `JNIEnv*`。
///
/// 源 VM 取**进程里第一个见过的 VM**（`ROOT_VM`）：attach 的线程通常不是当初发起
/// native 调用的那个线程，所以不能只看当前线程的上下文。
unsafe extern "C" fn vm_attach(_vm: *mut JavaVM, penv: *mut *mut c_void, _args: *mut c_void) -> jint {
    if penv.is_null() {
        return JNI_ERR;
    }
    let attached = ATTACHED.with(|a| a.borrow().is_some());
    if !attached {
        let root = ROOT_VM.load(Ordering::Relaxed);
        if root == 0 {
            return JNI_ERR; // 进程里还没有任何 VM，这个线程没法跑 Java
        }
        let v: &'static mut Vm = Box::leak(Box::new((*(root as *mut Vm)).fork()));
        ATTACHED.with(|a| *a.borrow_mut() = Some(v));
        ctx_push(v as *mut Vm);
    }
    *penv = env_ptr() as *mut c_void;
    JNI_OK
}

