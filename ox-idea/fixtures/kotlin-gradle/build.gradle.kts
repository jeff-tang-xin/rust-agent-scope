plugins {
    kotlin("jvm") version "2.0.21"
    application
}

group = "com.example"
version = "0.1.0"

repositories {
    mavenCentral()
}

dependencies {
    testImplementation(kotlin("test"))
    testImplementation("org.junit.jupiter:junit-jupiter:5.11.3")
}

kotlin {
    jvmToolchain(21)
}

application {
    mainClass.set("com.example.library.AppKt")
}

tasks.test {
    useJUnitPlatform()
    // `gradle test -PopenideaFail` runs the intentionally failing test.
    systemProperty("openidea.fail", project.hasProperty("openideaFail").toString())
}
