// RegisterNatives 路线（LWJGL 就是这么做的）：JNI_OnLoad 里一次性注册
#include <jni.h>
#include <stdio.h>
#include <string.h>

static jint JNICALL r_triple(JNIEnv *env, jclass c, jint x) {
    (void)env; (void)c;
    return x * 3;
}

static jstring JNICALL r_greet(JNIEnv *env, jclass c, jstring s) {
    (void)c;
    char buf[256];
    const char *n = (*env)->GetStringUTFChars(env, s, NULL);
    snprintf(buf, sizeof buf, "hello %s", n);
    (*env)->ReleaseStringUTFChars(env, s, n);
    return (*env)->NewStringUTF(env, buf);
}

JNIEXPORT jint JNICALL JNI_OnLoad(JavaVM *vm, void *reserved) {
    (void)reserved;
    JNIEnv *env = NULL;
    if ((*vm)->GetEnv(vm, (void **)&env, JNI_VERSION_1_6) != JNI_OK) return JNI_ERR;
    jclass c = (*env)->FindClass(env, "Reg");
    if (c == NULL) { printf("reg: FindClass failed\n"); return JNI_ERR; }
    static const JNINativeMethod ms[] = {
        {"triple", "(I)I", (void *)&r_triple},
        {"greet", "(Ljava/lang/String;)Ljava/lang/String;", (void *)&r_greet},
    };
    if ((*env)->RegisterNatives(env, c, ms, 2) != 0) {
        printf("reg: RegisterNatives failed\n");
        return JNI_ERR;
    }
    printf("reg: JNI_OnLoad + RegisterNatives ok\n");
    fflush(stdout);
    return JNI_VERSION_1_6;
}
