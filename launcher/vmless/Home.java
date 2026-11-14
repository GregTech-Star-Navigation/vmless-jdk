package vmless;

import java.io.File;
import java.util.Optional;

/** 从可执行文件位置推导 JDK home（<home>/bin/xxx.exe → <home>），并写入 java.home。 */
final class Home {
    static void install() {
        try {
            Optional<String> cmd = ProcessHandle.current().info().command();
            if (cmd.isPresent()) {
                File bin = new File(cmd.get()).getParentFile();
                if (bin != null && bin.getParentFile() != null) {
                    System.setProperty("java.home", bin.getParentFile().getAbsolutePath());
                }
            }
        } catch (Throwable ignore) {
            // ajva.home 保持原值
        }
    }
}
