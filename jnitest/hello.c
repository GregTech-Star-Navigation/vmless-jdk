// JNI 验收用的真 C 动态库（MSVC 编译成 hello.dll）。
//
// 覆盖：短名绑定（Java_Hello_*）、JNI_OnLoad、字符串、数组、异常、
// 全局引用、回调进 Java（静态 + 实例 + 浮点参数）。
#include <jni.h>
#include <stdio.h>
#include <string.h>

static int g_calls = 0;

JNIEXPORT void JNICALL Java_Hello_hi(JNIEnv *env, jclass cls) {
    (void)env; (void)cls;
    printf("hi from C\n");
}

JNIEXPORT jint JNICALL Java_Hello_add(JNIEnv *env, jclass cls, jint a, jint b) {
    (void)env; (void)cls;
    return a + b;
}

/* 字符串往返：取 modified UTF-8，拼个前缀再还回去（含中文/emoji） */
JNIEXPORT jstring JNICALL Java_Hello_echo(JNIEnv *env, jclass cls, jstring s) {
    (void)cls;
    if (s == NULL) return NULL;
    jsize len16 = (*env)->GetStringLength(env, s);
    jsize len8 = (*env)->GetStringUTFLength(env, s);
    const char *c = (*env)->GetStringUTFChars(env, s, NULL);
    char buf[512];
    snprintf(buf, sizeof buf, "C[%s]%d/%d", c, (int)len16, (int)len8);
    (*env)->ReleaseStringUTFChars(env, s, c);
    return (*env)->NewStringUTF(env, buf);
}

/* 数组：GetIntArrayElements 读+改，Release(0) 写回 */
JNIEXPORT jint JNICALL Java_Hello_arrayTest(JNIEnv *env, jclass cls, jintArray arr) {
    (void)cls;
    jsize n = (*env)->GetArrayLength(env, arr);
    jboolean isCopy = JNI_FALSE;
    jint *e = (*env)->GetIntArrayElements(env, arr, &isCopy);
    jint sum = 0;
    for (jsize i = 0; i < n; i++) sum += e[i];
    if (n > 0) { e[0] = 100; e[n - 1] = 200; }
    (*env)->ReleaseIntArrayElements(env, arr, e, 0);
    return sum;
}

/* 异常：native 里 ThrowNew，Java 侧 catch 得到 */
JNIEXPORT void JNICALL Java_Hello_throwIt(JNIEnv *env, jclass cls, jstring msg) {
    (void)cls;
    const char *c = (*env)->GetStringUTFChars(env, msg, NULL);
    jclass ex = (*env)->FindClass(env, "java/lang/RuntimeException");
    (*env)->ThrowNew(env, ex, c);
    (*env)->ReleaseStringUTFChars(env, msg, c);
}

/* 全局引用：建/比较/查类型/删，返回本库被调用的次数 */
JNIEXPORT jint JNICALL Java_Hello_refTest(JNIEnv *env, jclass cls, jobject o) {
    (void)cls;
    jobject g1 = (*env)->NewGlobalRef(env, o);
    jobject g2 = (*env)->NewGlobalRef(env, g1);
    if (g1 == NULL || g2 == NULL) return -1;
    if (!(*env)->IsSameObject(env, g1, o)) return -2;
    if (!(*env)->IsSameObject(env, g1, g2)) return -3;
    if ((*env)->GetObjectRefType(env, g1) != JNIGlobalRefType) return -4;
    (*env)->DeleteGlobalRef(env, g2);
    (*env)->DeleteGlobalRef(env, g1);
    if ((*env)->GetObjectRefType(env, g1) == JNIGlobalRefType) return -5;
    return ++g_calls;
}

/* 回调进 Java（实例方法，int 参数走 `...` 变参） */
JNIEXPORT void JNICALL Java_Hello_invokeOn(JNIEnv *env, jclass cls, jobject obj, jint v) {
    (void)cls;
    jclass c = (*env)->GetObjectClass(env, obj);
    jmethodID m = (*env)->GetMethodID(env, c, "set", "(I)V");
    if (m == NULL) return;
    (*env)->CallVoidMethod(env, obj, m, v);
    (*env)->DeleteLocalRef(env, c);
}

/* 回调进 Java（实例方法，**double 参数**走 `...` 变参 → 必须进 xmm 寄存器） */
JNIEXPORT jdouble JNICALL Java_Hello_invokeDouble(JNIEnv *env, jclass cls, jobject obj, jdouble v) {
    (void)cls;
    jclass c = (*env)->GetObjectClass(env, obj);
    jmethodID m = (*env)->GetMethodID(env, c, "scale", "(D)D");
    if (m == NULL) return -1.0;
    jdouble r = (*env)->CallDoubleMethod(env, obj, m, v);
    (*env)->DeleteLocalRef(env, c);
    return r;
}

/* 回调进 Java（**静态**方法，字符串参数）：验证 CallStaticVoidMethod 与 NewStringUTF 组合 */
JNIEXPORT void JNICALL Java_Hello_callback(JNIEnv *env, jclass cls) {
    jmethodID m = (*env)->GetStaticMethodID(env, cls, "onCall", "(Ljava/lang/String;)V");
    if (m == NULL) return;
    jstring s = (*env)->NewStringUTF(env, "来自 C 的回调");
    (*env)->CallStaticVoidMethod(env, cls, m, s);
    (*env)->DeleteLocalRef(env, s);
}

/* 字段读写 + 监视器：native 侧直接改 Java 对象的字段 */
JNIEXPORT jint JNICALL Java_Hello_fieldPlay(JNIEnv *env, jclass cls, jobject obj) {
    (void)cls;
    jclass c = (*env)->GetObjectClass(env, obj);
    jfieldID f = (*env)->GetFieldID(env, c, "value", "I");
    if (f == NULL) return -1;
    jint old = (*env)->GetIntField(env, obj, f);
    (*env)->SetIntField(env, obj, f, old + 7);
    if ((*env)->MonitorEnter(env, obj) != 0) return -2;
    if ((*env)->MonitorExit(env, obj) != 0) return -3;
    (*env)->DeleteLocalRef(env, c);
    return old;
}

/* JNI_OnLoad：短名绑定不注册任何东西，但要把版本号交回去 */
JNIEXPORT jint JNICALL JNI_OnLoad(JavaVM *vm, void *reserved) {
    (void)reserved;
    JNIEnv *env = NULL;
    if ((*vm)->GetEnv(vm, (void **)&env, JNI_VERSION_1_6) != JNI_OK) return JNI_ERR;
    printf("hello: JNI_OnLoad ok\n");
    fflush(stdout);
    return JNI_VERSION_1_6;
}
